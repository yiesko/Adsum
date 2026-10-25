//! The two long-running thread bodies `start` spawns: the hourly
//! database-refresh loop and the scan loop. Extracted from the inline
//! closures so the lifecycle module reads as lifecycle — the loop logic
//! (cadence, lanes, backoff, liveness logs) is unchanged, and each body
//! still owns its per-thread reuse buffers.

use std::collections::HashSet;
use std::time::Duration;

use adsum_types::AppId;

use crate::platform::memory::release_parse_arenas;
use crate::proc_start::process_start_ms;
use crate::refresh::{FetchOutcome, fetch_detectable_etag};
use crate::scratch::ScanScratch;
use crate::server::ProcessServer;
use crate::types::{Exec, epoch_ms};

use super::cadence::scan_cadence;
use super::lanes::{
  SnapshotDelivery, advance_snapshot_lane, apply_ignore_list, deliver_snapshot, first_sightings,
};
use super::workers::{is_shutting_down, track_shutdown_thread, wait_scan};

/// The hourly database/exclusions refresh loop (thread body).
pub(crate) fn run_refresh_loop(server: ProcessServer, db_url: String) {
  // Seeded from the startup fetch when available: the first check
  // is conditional like every other, instead of one guaranteed
  // redundant full rebuild per daemon lifetime.
  let mut etag = server.refresh.etag.clone();
  // Content hashes of the last built database: the raw hash guards
  // against byte-identical bodies (no parse at all), the trimmed
  // hash against volatile CDN bytes around identical games (parse,
  // but no rebuild). Either way an unchanged hour costs ~nothing.
  // Seeded from the startup fetch when available (see
  // `initial_db_content_hash`): without a seed the first check
  // always rebuilds once.
  let mut content_hash: Option<u64> = server.refresh.content_hash.map(|(raw, _)| raw);
  let mut trimmed_hash: Option<u64> = server.refresh.content_hash.map(|(_, trimmed)| trimmed);
  // Unlike the DB, exclusions are NOT fetched synchronously at
  // startup (tiny payload, empty = current behavior), so prime them
  // here instead of waiting an hour for the first set.
  //
  // Register before any blocking work: a shutdown landing during
  // the prime fetch leaves an unpark permit, so the hourly park
  // below returns at once instead of sleeping out the hour. A
  // shutdown that lands before this thread even starts exits at
  // the flag check without touching the network at all.
  track_shutdown_thread(&server.shutdown_wake);
  if is_shutting_down(&server.shutdown) {
    return;
  }
  server.refresh_exclusions();
  loop {
    // Pre-park flag check plus the unpark permit: either one exits
    // without waiting when shutdown is already requested.
    if is_shutting_down(&server.shutdown) {
      break;
    }
    // Hourly cadence as an interruptible park (not `sleep`): the
    // shutdown unpark bounds the wait instead of the hour.
    std::thread::park_timeout(Duration::from_secs(3600));
    if is_shutting_down(&server.shutdown) {
      break;
    }
    server.refresh_exclusions();
    match fetch_detectable_etag(&db_url, etag.as_deref(), content_hash, trimmed_hash) {
      Ok(FetchOutcome::Unchanged) => {
        tracing::info!(
          "[Process Scanner] DB check: unchanged (etag {})",
          etag.as_deref().unwrap_or("none")
        );
      }
      Ok(FetchOutcome::SameContent {
        etag: new_tag,
        content_hash: new_hash,
        trimmed_hash: new_trimmed,
      }) => {
        tracing::info!(
          "[Process Scanner] DB check: same content, new tag (etag {} -> {})",
          etag.as_deref().unwrap_or("none"),
          new_tag.as_deref().unwrap_or("none")
        );
        etag = new_tag;
        content_hash = Some(new_hash);
        trimmed_hash = Some(new_trimmed);
      }
      Ok(FetchOutcome::Updated {
        etag: new_tag,
        content_hash: new_hash,
        trimmed_hash: new_trimmed,
        detectable,
      }) => {
        tracing::info!(
          "[Process Scanner] DB updated: {} entries (etag {} -> {})",
          detectable.len(),
          etag.as_deref().unwrap_or("none"),
          new_tag.as_deref().unwrap_or("none")
        );
        // Commit validators only after the bundle is validated and
        // installed: a refused swap must not teach the next request
        // that never-installed data is current.
        if server.update_main_detectables(detectable) {
          etag = new_tag;
          content_hash = Some(new_hash);
          trimmed_hash = Some(new_trimmed);
          // The pre-swap generation is typically still pinned by an
          // in-flight tick at swap time, so the trim inside
          // `update_main_detectables` cannot release it. Trim once
          // more past any tick: production measurement showed the
          // second trim recovering ~30MB the first could not.
          // Delays the next refresh by one tick length; negligible
          // on the hourly cadence.
          if is_shutting_down(&server.shutdown) {
            break;
          }
          std::thread::park_timeout(Duration::from_secs(60));
          if is_shutting_down(&server.shutdown) {
            break;
          }
          release_parse_arenas();
          match adsum_telemetry::heap_numbers() {
            Some((live, free)) => tracing::info!(
              "[Process Scanner] post-retrim rss={:.1}MB (alloc live={:.1}MB free={:.1}MB)",
              adsum_telemetry::rss_bytes().unwrap_or(0) as f64 / 1_048_576.0,
              live as f64 / 1_048_576.0,
              free as f64 / 1_048_576.0
            ),
            None => {
              if let Some(rss) = adsum_telemetry::rss_bytes() {
                tracing::info!(
                  "[Process Scanner] post-retrim rss={:.1}MB",
                  rss as f64 / 1_048_576.0
                );
              }
            }
          }
        }
      }
      Err(err) => {
        tracing::warn!(
          "[Process Scanner] Error updating detectable database, retrying in 1h: {}",
          err
        );
      }
    }
  }
}

/// The scan loop (thread body): one tick per cadence, early wakes from
/// the proc-events watcher, deltas into the snapshot lane.
pub(crate) fn run_scan_loop(server: ProcessServer, wait_time: Duration) {
  // Register for early wakeups: the proc-events watcher unparks us
  // the moment a tracked game exits (Linux only; elsewhere None and
  // the cadence below is a plain sleep). `shutdown` unparks this
  // same slot, so teardown never waits out the cadence either.
  *server.scan_wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current());
  // Idle backoff state: consecutive ticks with no games detected.
  let mut idle_ticks: u32 = 0;
  // Game ids already announced this boot (first-sighting INFO below).
  // `AppId` keys: first-sighting inserts are refcount bumps off the
  // entries' own ids (see `first_sightings`), never byte copies.
  let mut seen_ids: HashSet<AppId> = HashSet::new();
  // Last detection snapshot forwarded to the bridge, as sorted
  // (app id, pid) pairs. The bridge dedups repeats internally, so
  // re-sending an identical table every tick only costs wakeups,
  // channel traffic and locks for zero effect — forward deltas only.
  // Transitions (including pid changes on restart) still send the
  // full current table, exactly like before.
  let mut last_emitted: Vec<(AppId, u64)> = Vec::new();
  // The per-tick snapshot buffer: hoisted out of the loop and
  // cleared/refilled in place, so an emitted tick swaps tables with
  // `last_emitted` instead of allocating a fresh Vec (and a fresh
  // `String` per id) every tick. Ids are `AppId` clones — refcount
  // bumps sharing the entry's allocation.
  let mut snapshot: Vec<(AppId, u64)> = Vec::new();
  // First-tick liveness proof (INFO, once): a scan thread that never
  // completes tick one is otherwise indistinguishable from an idle
  // one without a debug build.
  let mut first_tick = true;
  // Tick reuse buffers, owned by this loop: the process list backing,
  // every `Exec` slot and both read scratches survive across ticks, so
  // a warm tick allocates nothing per process (see `read_exec_into`).
  let mut tick_processes: Vec<Exec> = Vec::new();
  let mut tick_scratch = ScanScratch::default();
  // Run the process scan repeatedly (base cadence, stretched while idle)
  loop {
    // Directed shutdown: the unpark below only shortens the cadence
    // wait; the flag is the actual exit (checked here and after the
    // wait, since EXIT wakes share the same unpark).
    if is_shutting_down(&server.shutdown) {
      break;
    }
    *server.last_scan.lock().unwrap_or_else(|e| e.into_inner()) = std::time::Instant::now();
    let mut detected = match server.scan_for_processes(&mut tick_processes, &mut tick_scratch) {
      Ok(detected) => detected,
      Err(err) => {
        tracing::warn!(
          "[Process Scanner] Error while scanning processes, retrying: {}",
          err
        );
        wait_scan(wait_time);
        continue;
      }
    };
    // Coexistence filter: ignored app IDs behave as absent, so a richer
    // publisher elsewhere owns the slot (clears flow normally).
    let before = detected.len();
    detected = apply_ignore_list(detected, &server.ignored_ids);
    if detected.len() != before {
      tracing::debug!(
        "[Process Scanner] Ignored {} detected game(s)",
        before - detected.len()
      );
    }
    // First-tick liveness proof (INFO, once per boot): proves the
    // loop enumerated and classified, whatever it found. A boot
    // with games running that reports 0 here is a wedged scan,
    // not an idle one — distinguishable without debug builds.
    if first_tick {
      first_tick = false;
      tracing::info!(
        "[Process Scanner] First tick complete: {} game(s)",
        detected.len()
      );
    }
    // First sightings this boot, at INFO: without this, a daemon
    // whose bridge path goes quiet is indistinguishable from a
    // blind one without a debug build. Bounded: one line
    // per game id per boot, same cadence as bridge publishes.
    // Each line carries provenance (which matcher won) and latency
    // (how long the process had been alive when first classified):
    // rare by construction, nothing here runs on the hot path.
    for game in first_sightings(&mut seen_ids, &detected) {
      let age_ms = process_start_ms(game.pid).map(|started| epoch_ms().saturating_sub(started));
      match age_ms {
        Some(age) => tracing::info!(
          "[Process Scanner] Detected: {} ({}, pid {}) via {}, process age {}ms",
          game.entry.name,
          game.entry.id,
          game.pid,
          game.source.as_str(),
          age
        ),
        None => tracing::info!(
          "[Process Scanner] Detected: {} ({}, pid {}) via {}",
          game.entry.name,
          game.entry.id,
          game.pid,
          game.source.as_str()
        ),
      }
    }
    // Track live game pids for the proc-events watcher: only THEIR
    // exits wake us early (a build storm's exits never cause a scan).
    // Clear and refill (not wholesale replace): the set is rebuilt
    // from the live table every tick anyway, so swapping in a fresh
    // `HashSet` allocated a new table per tick for identical content.
    // A watcher insert racing this write can be dropped, but the next
    // tick re-derives it from the live table while the game still
    // runs — and a lost EXIT entry heals the same way. Bounded
    // staleness (≤1 tick), no growth, no liveness probe per entry.
    //
    // Capacity is kept honest too: after a process spike the table
    // would otherwise sit at its high-water bucket count forever.
    {
      let mut pids = server
        .detected_pids
        .lock()
        .unwrap_or_else(|e| e.into_inner());
      pids.clear();
      pids.extend(detected.iter().map(|game| game.pid));
      let target = if pids.capacity() > 64 && pids.len() * 4 < pids.capacity() {
        (pids.len() * 2).max(16)
      } else {
        0
      };
      if target != 0 {
        pids.shrink_to(target);
      }
    }
    // Forward on change only (see `last_emitted`): identical tables
    // are already fully represented downstream (`note_scan` entries
    // persist, `last_process` dedups), so skipping them changes
    // nothing observable — it just stops waking the bridge thread.
    // The EXEC fast path can publish a game no poll ever observes
    // (sub-tick lifetime + missed EXIT): `scan_dirty` forces one full
    // emission after any EXEC publication, so its clear still flows.
    snapshot.clear();
    snapshot.extend(
      detected
        .iter()
        .map(|game| (game.entry.id.clone(), game.pid)),
    );
    snapshot.sort();
    let changed = snapshot != last_emitted;
    let forced = server
      .scan_dirty
      .swap(false, std::sync::atomic::Ordering::AcqRel);
    let emit = changed || forced;
    // Advance only when the tick emits: `advance_snapshot_lane` computes
    // the delta from the recorded table, delivers it, and records the new
    // one only when the delivery is complete.
    let delivery = if emit {
      advance_snapshot_lane(&mut last_emitted, &mut snapshot, |removed| {
        deliver_snapshot(
          &server.event_sender,
          removed,
          &detected,
          detected.is_empty(),
        )
      })
    } else {
      SnapshotDelivery::Complete
    };
    if matches!(delivery, SnapshotDelivery::Closed) {
      tracing::warn!("[Process Scanner] Event receiver gone, shutting down scan");
      return;
    }
    if matches!(delivery, SnapshotDelivery::Shed) {
      // The consumer still holds the previous table: `advance_snapshot_lane`
      // did not record this one, so the next tick re-emits it whole.
      // Nothing is lost, because `last_emitted` was never advanced past
      // an undelivered tick.
      tracing::debug!("[Process Scanner] Shed a snapshot event; next tick re-emits");
    }

    // Idle backoff: consecutive empty ticks stretch the cadence
    // (base → 30s cap), but only while the EXEC watcher is live to
    // wake us early. Game START arrives via EXEC instantly and EXITs
    // of tracked games unpark us — polling only backstops what the
    // watcher cannot (untracked exits, DB refreshes). Polling-only
    // paths (other OSes, disabled/failed watcher) keep the base
    // cadence so a short interval never silently grows to 30s.
    // Any detection or early wake resets.
    if detected.is_empty() {
      idle_ticks = idle_ticks.saturating_add(1);
    } else {
      idle_ticks = 0;
    }
    let cadence = scan_cadence(
      wait_time,
      idle_ticks,
      server
        .watcher_live
        .load(std::sync::atomic::Ordering::Acquire),
    );
    let wait_start = std::time::Instant::now();
    if is_shutting_down(&server.shutdown) {
      break;
    }
    wait_scan(cadence);
    if is_shutting_down(&server.shutdown) {
      break;
    }
    if wait_start.elapsed() < cadence.mul_f32(0.9) {
      idle_ticks = 0;
    }
  }
}
