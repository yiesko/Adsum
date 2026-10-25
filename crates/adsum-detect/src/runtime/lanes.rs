//! The event lanes: everything between "a tick produced hits" and "the
//! bridge received events" — the snapshot lane (delta, delivery
//! accounting, shed semantics) and the EXEC fast-path publication, plus
//! the two result post-processors the scan loop runs before the lane
//! (first-sighting logs and the ignore-list filter).
//!
//! The ordering rule this module exists to keep: the recorded table
//! advances ONLY on a complete delivery, so a shed tick re-emits whole
//! on the next tick instead of losing a clear (the ghost-card fix).

use std::collections::HashSet;

use adsum_telemetry::SendChecked;
use adsum_types::AppId;

use crate::types::{ProcessDetectedEvent, ScannedHit};

/// Whether a failed `event_sender.send` must end the calling thread.
///
/// The rule that makes the snapshot lane safe under a lossy channel.
///
/// The old order recorded the new table in `last_emitted` **before**
/// sending its events. With an unbounded channel that was invisible
/// (sends only fail when the receiver is gone, which ends the thread), so
/// the bug was latent. The moment a send can be dropped — a bounded
/// channel, `send_checked` shedding past its cap — the new state is
/// recorded while the event that carries it is lost, and the next tick
/// computes its delta against the new table: the vanished slot never gets
/// its `Removed`, and the consumer is left holding a card it was told to
/// forget (a ghost clear) or never told about (a ghost card).
///
/// So the table is only recorded once every event of the tick reached the
/// channel. A shed tick leaves the previous table in place and is
/// re-emitted whole on the next tick — the same self-healing the flood
/// guard already relies on, and downstream dedups repeats per app id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotDelivery {
  /// Every event of the tick was accepted: the caller may record the new
  /// table as emitted.
  Complete,
  /// At least one event was shed: the consumer still holds the previous
  /// table, so the caller must keep it and let the next tick re-emit.
  Shed,
  /// The receiver is gone (shutdown).
  Closed,
}

/// Backlog budget for the snapshot lane's events.
///
/// Events are tiny (`ScannedHit` is 32 B plus an `Arc`), the lane feeds a
/// bridge pump that drains continuously, and a shed is repaired by the
/// next tick — so this is a budget that bounds the *blast radius* of a
/// stalled consumer, not a queue that must never fill.
pub(crate) const SNAPSHOT_EVENT_CAP: usize = 128;

/// Send one snapshot's events (removals, then detections, then the
/// empty-table clear) and report whether the tick was fully delivered.
///
/// Stops early on a shed: piling more events onto a full backlog cannot
/// make the tick complete, and the next tick re-emits from the previous
/// table anyway.
pub(crate) fn deliver_snapshot(
  sender: &adsum_telemetry::GaugeSender<ProcessDetectedEvent>,
  removed: &[(AppId, u64)],
  detected: &[ScannedHit],
  cleared: bool,
) -> SnapshotDelivery {
  for (id, pid) in removed {
    match sender.send_checked(
      ProcessDetectedEvent::Removed {
        // Refcount clone, not a byte copy: `AppId` is an `Arc<str>`
        // newtype, so a removal event shares the snapshot lane's id.
        id: id.clone(),
        pid: *pid,
      },
      SNAPSHOT_EVENT_CAP,
    ) {
      SendChecked::Sent => {}
      SendChecked::Shed => return SnapshotDelivery::Shed,
      SendChecked::Closed => return SnapshotDelivery::Closed,
    }
  }
  for game in detected {
    match sender.send_checked(
      ProcessDetectedEvent::detected(game.clone()),
      SNAPSHOT_EVENT_CAP,
    ) {
      SendChecked::Sent => {}
      SendChecked::Shed => return SnapshotDelivery::Shed,
      SendChecked::Closed => return SnapshotDelivery::Closed,
    }
  }
  if cleared {
    match sender.send_checked(ProcessDetectedEvent::cleared(), SNAPSHOT_EVENT_CAP) {
      SendChecked::Sent => {}
      SendChecked::Shed => return SnapshotDelivery::Shed,
      SendChecked::Closed => return SnapshotDelivery::Closed,
    }
  }
  SnapshotDelivery::Complete
}

/// Backlog budget for the EXEC fast path's publication. Matches the
/// snapshot lane: events are tiny, the consumer drains continuously, and
/// a shed is repaired by the forced full emission `scan_dirty` triggers.
///
/// The EXEC fast path is a netlink watcher, so both this and the
/// publisher below are dead weight off Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const EXEC_EVENT_CAP: usize = 128;

/// Publish one EXEC classification, capacity-checked.
///
/// A shed is **not** a failure: the caller sets `scan_dirty` before
/// publishing, so a lost hint costs one poll interval and then the whole
/// table is re-emitted. Reporting it as a success is what lets the lane
/// carry on instead of tearing itself down over a dropped best-effort
/// event. Only a gone receiver ends the lane — with no receiver there is
/// nothing to drain and nothing to repair.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn publish_exec_hit(
  sender: &adsum_telemetry::GaugeSender<ProcessDetectedEvent>,
  hit: ScannedHit,
  cap: usize,
) -> bool {
  match sender.send_checked(ProcessDetectedEvent::detected(hit), cap) {
    SendChecked::Sent | SendChecked::Shed => true,
    SendChecked::Closed => false,
  }
}

/// Advance the snapshot lane for one tick: compute the delta against the
/// recorded table, deliver it, and record the new table **only when the
/// delivery was complete**.
///
/// This is the whole E2 fix, isolated so the ordering is testable without
/// a channel. `deliver` is injected so a test can force each outcome; the
/// delta is always computed from the *recorded* table, which is what makes
/// a shed tick self-healing rather than a silent loss.
///
/// Both tables are the caller's reuse buffers: a complete delivery swaps
/// them (the recorded table becomes the new snapshot, the scratch buffer
/// inherits the old table and is cleared on the next build), so recording
/// a tick allocates nothing. A shed leaves both untouched — the scratch
/// still holds the undelivered table, and the next tick rebuilds over it.
pub(crate) fn advance_snapshot_lane<F>(
  last_emitted: &mut Vec<(AppId, u64)>,
  snapshot: &mut Vec<(AppId, u64)>,
  deliver: F,
) -> SnapshotDelivery
where
  F: FnOnce(&[(AppId, u64)]) -> SnapshotDelivery,
{
  let removed = removed_slots(last_emitted, snapshot);
  let delivery = deliver(&removed);
  if matches!(delivery, SnapshotDelivery::Complete) {
    std::mem::swap(last_emitted, snapshot);
  }
  delivery
}

/// `(app id, pid)` pairs present in the previous snapshot but absent now.
/// Exact-tuple matching (not id-only): a pid replacement reports the old
/// pair as removed before the new detection is published, so the bridge
/// rotates the card instead of keeping a stale pid. Tables are tiny
/// (co-running games), so the quadratic scan allocates nothing.
pub(crate) fn removed_slots(
  previous: &[(AppId, u64)],
  current: &[(AppId, u64)],
) -> Vec<(AppId, u64)> {
  previous
    .iter()
    .filter(|pair| !current.contains(pair))
    .cloned()
    .collect()
}

/// Cap on remembered application ids per boot.
///
/// The set exists only to log a first sighting once, not to remember every
/// id a long-lived daemon ever saw: a daemon that detects a thousand
/// distinct games over weeks would otherwise grow this set forever.
pub(crate) const MAX_SEEN_IDS: usize = 1024;

/// First sightings this boot: entries of `detected` not yet in `seen`
/// (which is updated in place). The scan loop logs these at INFO so a
/// silent daemon is distinguishable from a blind one without a debug
/// build; the bridge still owns publish/dedup logging downstream.
///
/// `seen` is bounded: past `MAX_SEEN_IDS` it is dropped whole rather
/// than growing without limit, so a repeat INFO may surface after enough
/// id churn. That is the accepted trade for constant memory — the log
/// line is diagnostic, never state.
pub fn first_sightings<'a>(
  seen: &mut HashSet<String>,
  detected: &'a [ScannedHit],
) -> Vec<&'a ScannedHit> {
  if seen.len() >= MAX_SEEN_IDS {
    seen.clear();
  }
  let mut first = Vec::new();
  for game in detected {
    // `contains` first: the common case is a game already seen this boot,
    // and building the key only for `insert` to discard it allocated for
    // nothing.
    if seen.contains(game.entry.id.as_ref()) {
      continue;
    }
    seen.insert(game.entry.id.to_string());
    first.push(game);
  }
  first
}

/// Drop scan results whose application id is ignored, preserving order.
/// Pure: an ignored-only scan yields an empty vec, which the scan thread
/// already turns into the normal null event (clear). Tested below.
pub fn apply_ignore_list(
  detected: Vec<ScannedHit>,
  ignored_ids: &HashSet<String>,
) -> Vec<ScannedHit> {
  if ignored_ids.is_empty() {
    return detected;
  }
  detected
    .into_iter()
    .filter(|game| {
      let id: &str = &game.entry.id;
      !ignored_ids.contains(id)
    })
    .collect()
}

/// Whether an EXEC fast-path hit must be skipped: ignored app IDs behave
/// as absent everywhere (parity with the polling path's
/// [`apply_ignore_list`]). One `HashSet` lookup; empty set early-outs via
/// the caller's check below (`contains` on empty is already cheap, but the
/// intent reads explicitly at the call site).
/// Linux-only caller (`spawn_proc_watcher`): allow dead code elsewhere,
/// same as [`ProcessServer::should_wake_on_exit`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn exec_hit_ignored(ignored_ids: &HashSet<String>, hit: &ScannedHit) -> bool {
  ignored_ids.contains(hit.entry.id.as_ref())
}
