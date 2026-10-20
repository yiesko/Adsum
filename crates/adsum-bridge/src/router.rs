//! Activity routing: game commands and scanner reports in, fan-out
//! decisions out.
//!
//! Decides what each input means (new publish, duplicate flood,
//! genuine clear, handoff between SDK and generic detection) and drives
//! the consumer fan-out. Transport-agnostic: pumps feed it commands,
//! it answers through the [`Shared`] state.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use adsum_protocol::commands::{self, CachedActivity};
use adsum_types::cmd::ActivityCmd;
use adsum_types::{AppId, SocketId};
use tokio::sync::mpsc;

use super::bridge::Shared;
use super::handoff::{
  ClearReason, ProcInput, ScannedGame, is_process_alive, track_process_publication,
};
use super::replay::prune_cache;

/// Session-boundary reason for a process-table event: the table went
/// from empty to non-empty (`game-start`) or back to empty (`game-end`).
/// Repeats within a state stay quiet.
pub(crate) fn table_transition(had_games: bool, input: &ProcInput) -> Option<&'static str> {
  match input {
    ProcInput::Detected(_) if !had_games => Some("game-start"),
    ProcInput::Cleared if had_games => Some("game-end"),
    _ => None,
  }
}

/// Queued depth of a census sender (`max - free`).
pub(crate) fn channel_depth<T>(tx: Option<&mpsc::Sender<T>>) -> usize {
  tx.map(|tx| tx.max_capacity().saturating_sub(tx.capacity()))
    .unwrap_or(0)
}

/// Whether a command is a genuine clear from a real connection (explicit
/// `activity: null` + nonzero pid). A command that omits the field, or never
/// identified a game (pid 0), is not a clear.
pub(crate) fn is_genuine_clear(cmd: &ActivityCmd) -> bool {
  let Some(args) = cmd.args.as_ref() else {
    return false;
  };
  args.pid.is_some_and(|pid| pid != 0) && matches!(args.activity, Some(None))
}

/// Build the generic process-detection payload for a scanned game.
pub(crate) fn generic_payload(game: &ScannedGame) -> Option<Arc<CachedActivity>> {
  let payload_struct = commands::ProcessPayload {
    activity: commands::ProcessActivity {
      application_id: game.id.clone(),
      name: game.name.clone(),
      timestamps: commands::ProcessTimestamps { start: game.start },
      r#type: 0,
      metadata: HashMap::new(),
      flags: 0,
    },
    pid: game.pid,
    socket_id: SocketId::from(&game.id),
  };
  // Fixed shapes: practically unreachable encode failure surfaces as
  // `None` so callers log and drop instead of shipping an empty frame.
  let activity_json = serde_json::to_vec(&payload_struct.activity).ok()?;
  // Provenance for snapshots: the matcher's source travels on the game,
  // latency is process age at this (first) publish. Rare path only —
  // never per tick per process.
  let now_ms = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .ok()
    .and_then(|age| u64::try_from(age.as_millis()).ok())
    .unwrap_or(0);
  let provenance = Some(commands::ActivityProvenance {
    // Borrowed static wire id: the publish path allocates nothing to say
    // where the card came from.
    source: Cow::Borrowed(game.source.as_str()),
    latency_ms: game
      .process_start_ms
      .map(|started| now_ms.saturating_sub(started)),
  });
  Some(Arc::new(commands::CachedActivity {
    json: serde_json::to_string(&payload_struct)
      .ok()
      .map(tungstenite::Utf8Bytes::from)?,
    msgpack: rmp_serde::to_vec_named(&payload_struct)
      .ok()
      .map(bytes::Bytes::from)?,
    // Always built with `activity: Some` above.
    is_clear: false,
    activity_json: bytes::Bytes::from(activity_json),
    provenance,
    pid: game.pid,
  }))
}

/// Remove one process publication only when the stored pid matches the
/// removal event (pid reuse / EXEC-vs-poll race must not clear a newer
/// detection). Single map access: callers must hold one guard across the
/// check, never compare-then-remove under separate locks.
pub(crate) fn take_matching_process(
  last_process: &mut HashMap<AppId, u64>,
  app_id: &AppId,
  pid: u64,
) -> Option<u64> {
  if last_process.get(app_id).is_some_and(|known| *known == pid) {
    last_process.remove(app_id)
  } else {
    None
  }
}

/// Consume the outstanding process publications for clearing, if any.
/// Returns `(pid, app_id)` pairs, sorted for deterministic clears.
/// Single-shot by construction (`drain`): repeated null scans clear once
/// and then skip.
pub(crate) fn take_process_clear(shared: &Shared) -> Vec<(u64, AppId)> {
  let mut outstanding: Vec<(u64, AppId)> = shared
    .last_process
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .drain()
    .map(|(app_id, pid)| (pid, app_id))
    .collect();
  outstanding.sort();
  outstanding
}

impl Shared {
  /// IPC-wins handoff: a live SDK presence takes over this app slot
  /// from generic detection (last publisher wins across companions).
  /// Logs takeovers once — fresh claims and owner changes alike (steady
  /// republishes with ticking timestamps stay quiet) — naming the
  /// suppressed generic so the journal shows which side owns the slot
  /// and why.
  pub(crate) fn note_sdk_publish(&self, cmd: &ActivityCmd) {
    let args = cmd.args.as_ref();
    let pid = args.and_then(|args| args.pid).unwrap_or_default();
    if let Some(app) = args
      .and_then(|args| args.activity.as_ref())
      .and_then(|activity| activity.as_ref())
      .and_then(|activity| activity.application_id.clone())
    {
      let (previous, generic) = {
        let mut handoff = self.handoff.lock().unwrap_or_else(|e| e.into_inner());
        let previous = handoff.owner_of(&app);
        handoff.note_publish(&app, pid);
        let generic = handoff
          .resume_for(&app)
          .map(|game| game.name.clone())
          .unwrap_or_default();
        (previous, generic)
      };
      // Same owner republishing: quiet (timestamps tick every second).
      if previous != Some(pid) {
        match (previous, generic.is_empty()) {
          (None, true) => {
            tracing::info!("[bridge] SDK presence for {app} (pid {pid}) now owns its slot");
          }
          (None, false) => {
            tracing::info!(
              "[bridge] SDK presence for {app} (pid {pid}) took over slot from generic {generic}"
            );
          }
          (Some(old), _) => {
            tracing::info!(
              "[bridge] SDK presence for {app} (pid {pid}) took over slot from pid {old}"
            );
          }
        }
      }
    }
  }

  /// Compare the current fingerprint against the cached entry, so first
  /// publishes, real changes and effective clears still log once.
  /// Byte compare: both sides come from the same serializer and the
  /// stored bytes travel with the build — no re-serialize, no re-parse.
  pub(crate) fn activity_changed(&self, socket_id: &SocketId, fingerprint: Option<&[u8]>) -> bool {
    // Clone the two small shape facts we need (flag + bytes) and let the
    // guard drop at the semicolon: nothing below holds the map lock.
    let cached = self
      .cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(socket_id)
      .map(|(entry, _)| (entry.is_clear, entry.activity_json.clone()));
    match (cached, fingerprint) {
      (None, None) => false,
      (None, Some(_)) => true,
      (Some((is_clear, _)), None) => !is_clear,
      (Some((is_clear, stored)), Some(fp)) => is_clear || stored.as_ref() != fp,
    }
  }

  /// A genuine clear (null activity) from a real connection means the
  /// SDK source went away: hand the slot back to generic detection so
  /// the scanner re-asserts the still-running game. pid == 0 means no
  /// game was ever identified — ignore those, or every fresh
  /// connection would flap the display.
  pub(crate) fn resume_after_clear(&self, cmd: &ActivityCmd, changed: bool) {
    if !is_genuine_clear(cmd) {
      return;
    }
    let pid = cmd
      .args
      .as_ref()
      .and_then(|args| args.pid)
      .unwrap_or_default();
    // Borrowed once and shared by both arms: the handoff takes `&str`, and
    // the log arm only formats it, so no owned copy is needed anywhere here.
    let app = cmd.application_id.as_deref();
    let resume: Vec<ScannedGame> = {
      let mut handoff = self.handoff.lock().unwrap_or_else(|e| e.into_inner());
      match app {
        Some(app) if handoff.note_clear(app, pid) => handoff.resume_for(app).into_iter().collect(),
        Some(_) => Vec::new(),
        // No app id: abrupt close (socket died without CLEAR).
        // Release every slot this pid owned, or their generics stay
        // suppressed by a dead owner forever.
        None => handoff
          .note_clear_pid(pid)
          .into_iter()
          .filter_map(|app| handoff.resume_for(app.as_ref()))
          .collect(),
      }
    };
    let mut resumed = 0;
    for game in resume.into_iter().filter(|game| is_process_alive(game.pid)) {
      self.resume_generic(&game);
      resumed += 1;
    }
    if changed {
      match app {
        Some(app) => tracing::info!(
          "[bridge] Source cleared for {app} (pid {pid}), resuming {resumed} slot(s)"
        ),
        // No app id: abrupt close (socket died without CLEAR) — the
        // release names the dead owner instead of a slot.
        None => tracing::info!(
          "[bridge] Owner pid {pid} went away without CLEAR, resuming {resumed} slot(s)"
        ),
      }
    } else {
      tracing::debug!("[bridge] Duplicate clear ignored (pid {pid})");
    }
  }

  /// Handle one `SET_ACTIVITY` command: fingerprint, flood-guard,
  /// handoff, change detection, clear resume, envelope build, fan-out.
  pub(crate) async fn handle_set_activity(&self, mut cmd: ActivityCmd) {
    // Defensive: IPC rejects an absent `args` before this path; WS forwards
    // one for the bridge to drop here. Both reject an absent `activity`
    // field (WS only while `SET_ACTIVITY` is enabled). Neither is a clear.
    let Some(args) = cmd.args.as_ref() else {
      tracing::warn!("[bridge] Invalid activity command, skipping");
      return;
    };
    if args.activity.is_none() {
      tracing::warn!("[bridge] Missing activity field, skipping");
      return;
    }
    // Fingerprint first (runs `fix()`): flood-dropped publishes return
    // before the envelope (JSON + MessagePack) is ever built.
    let fingerprint = commands::activity_fingerprint(&mut cmd);
    let pid = cmd
      .args
      .as_ref()
      .and_then(|args| args.pid)
      .unwrap_or_default();
    if self.flood_dropped(&cmd, fingerprint.as_ref()) {
      let app_key = cmd.application_id.as_deref().unwrap_or("");
      tracing::debug!("[bridge] Dropping duplicate SET_ACTIVITY (app {app_key}, pid {pid})");
      return;
    }
    // An `AppId` clone is a refcount bump, not a byte copy, so this is
    // cheap — and it keeps `app_key` alive for the log at the bottom
    // without borrowing `cmd` past the move below.
    let app_key = cmd.application_id.as_deref().unwrap_or("");
    let activity = cmd
      .args
      .as_ref()
      .and_then(|args| args.activity.as_ref())
      .and_then(|activity| activity.as_ref());
    // One socket id per publish, shared by the change check below and the
    // fan-out: the decimal used to be rendered twice, each time through a
    // heap-allocated `String`.
    let socket_id = SocketId::from(pid);
    let changed = self.activity_changed(&socket_id, fingerprint.as_deref());
    // Freshly built (sole owner): SDK provenance is stamped on the payload
    // below, so snapshots tell client cards apart from detected generics.
    self.note_sdk_publish(&cmd);
    // NOTE: no ignore-list filtering here by design. Forwarded client
    // frames are indistinguishable on this path — filtering would kill
    // the companion this bridge exists to carry.
    match activity {
      Some(activity) => {
        if changed {
          tracing::info!(
            "[bridge] Published: {} (app {}, pid {})",
            activity.display_name(),
            activity.application_id.as_deref().unwrap_or("?"),
            pid
          );
        } else {
          tracing::debug!(
            "[bridge] Published: {} (app {}, pid {})",
            activity.display_name(),
            activity.application_id.as_deref().unwrap_or("?"),
            pid
          );
        }
      }
      None => {
        // Broadcast clears here always originate from a client frame
        // (abrupt closes send none) — reason is sdk-clear by
        // construction; other reasons are tagged at their own sites.
        let reason = ClearReason::SdkClear.as_str();
        if changed {
          tracing::info!("[bridge] Published clear for {app_key} (pid {pid}): {reason}");
        } else {
          tracing::debug!("[bridge] Published clear for {app_key} (pid {pid}): {reason}");
        }
      }
    }
    // A genuine clear (null activity) from a real connection means the
    // SDK source went away: hand the slot back to generic detection.
    self.resume_after_clear(&cmd, changed);

    // Envelope build last, and only when the payload is actually used: it
    // is pure, so it is the only step that may sit after the side effects
    // above, and it consumes `cmd`. An identical republish discards the
    // payload either way, so it never pays for the build.
    if changed {
      let Some(mut payload) = commands::cached_activity(cmd, fingerprint) else {
        tracing::warn!("[bridge] Invalid activity command, skipping");
        return;
      };
      if let Some(payload) = Arc::get_mut(&mut payload) {
        payload.provenance = Some(commands::ActivityProvenance {
          source: Cow::Borrowed("sdk"),
          latency_ms: None,
        });
      }
      self.broadcast_activity(payload, socket_id);
    } else {
      // Identical republishes change nothing observable: the replay cache
      // already holds these exact bytes (late joiners replay them) and the
      // refresh re-asserts them — so skip the fan-out. First publishes,
      // real changes and effective clears always pass.
      tracing::debug!(
        "[bridge] Already published identical activity (app {app_key}, pid {pid}), skipping fan-out"
      );
    }
  }

  /// Re-assert generic process presence after its source cleared. The
  /// scanner only emits on *changes*, so without this the slot would stay
  /// dark until the next game switch.
  pub(crate) fn resume_generic(&self, game: &ScannedGame) {
    track_process_publication(
      &mut self.last_process.lock().unwrap_or_else(|e| e.into_inner()),
      game.id.clone(),
      game.pid,
    );
    tracing::debug!(
      "[bridge] Resuming generic presence for {} ({})",
      game.name,
      game.id
    );
    let Some(payload) = generic_payload(game) else {
      tracing::warn!("[bridge] Dropping unencodable generic payload");
      self.dropped_broadcasts.fetch_add(1, Ordering::Relaxed);
      return;
    };
    self.broadcast_activity(payload, SocketId::from(&game.id));
  }

  /// Broadcast an activity payload, updating the replay cache (clears
  /// evict) and marking the snapshot dirty — never persisting inline.
  pub(crate) fn broadcast_activity(&self, payload: Arc<CachedActivity>, socket_id: SocketId) {
    // Keep the replay cache in sync, pruning cleared activities. The flag
    // travels with the build — no re-parse of our own serialization.
    let is_clear = payload.is_clear;
    {
      let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
      if is_clear {
        cache.remove(&socket_id);
      } else {
        let seq = self
          .activity_seq
          .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
          + 1;
        cache.insert(socket_id, (Arc::clone(&payload), seq));
        prune_cache(&mut cache);
      }
    }
    self.send_to_all(&payload);
    self.dirty.store(true, Ordering::Relaxed);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::handoff::HandoffState;
  use adsum_types::DetectSource;

  /// Epoch millis for latency fixtures.
  fn now_ms() -> u64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .ok()
      .and_then(|age| u64::try_from(age.as_millis()).ok())
      .unwrap_or(0)
  }

  /// Generic payloads stamp detection provenance for snapshots: the
  /// matcher's source travels on the card, latency is process age at
  /// first publish.
  #[test]
  fn generic_payload_stamps_provenance() {
    let game = ScannedGame {
      id: AppId::from("12345"),
      name: Arc::from("Source Game"),
      pid: u64::from(std::process::id()),
      start: 0,
      source: DetectSource::SteamAppId,
      process_start_ms: Some(now_ms().saturating_sub(5000)),
    };
    let payload = generic_payload(&game).expect("builds");
    let provenance = payload.provenance.clone().expect("stamped");
    assert_eq!(provenance.source, "steam-app-id");
    let latency = provenance.latency_ms.expect("latency");
    assert!(
      (5000..=60_000).contains(&latency),
      "latency {latency}ms should be ~5s"
    );
  }

  /// F2: the matcher source travels as a `DetectSource`, not a `String`,
  /// and the provenance keeps the wire identifier without re-allocating.
  ///
  /// Before, the daemon stringified the enum at the scanner/bridge
  /// boundary, the bridge cloned that `String` per event, and the
  /// provenance cloned it again. The enum is `Copy`, and the provenance
  /// borrows the static wire identifier — so detection provenance costs
  /// nothing on the publish path.
  #[test]
  fn provenance_source_travels_as_the_matcher_enum() {
    for (source, expected) in [
      (DetectSource::Automaton, "automaton"),
      (DetectSource::SteamAppId, "steam-app-id"),
      (DetectSource::EpicAppId, "epic-app-id"),
      (DetectSource::ProtonAutomaton, "proton-automaton"),
      (DetectSource::SteamLibrary, "steam-library"),
      (DetectSource::ExeStem, "exe-stem"),
      (DetectSource::Folder, "folder"),
      (DetectSource::CwdJoined, "cwd-joined"),
    ] {
      let game = ScannedGame {
        id: AppId::from("12345"),
        name: Arc::from("Source Game"),
        pid: u64::from(std::process::id()),
        start: 0,
        source,
        process_start_ms: Some(now_ms().saturating_sub(5000)),
      };
      let payload = generic_payload(&game).expect("builds");
      let provenance = payload.provenance.clone().expect("stamped");
      assert_eq!(
        &*provenance.source, expected,
        "wire id changed for {source:?}"
      );
      // The point of the `Cow`: a matcher source borrows its static wire
      // id, so a publish allocates nothing to say where the card came
      // from. Value equality alone would pass with an owned `String`.
      assert!(
        matches!(provenance.source, Cow::Borrowed(_)),
        "{source:?} must borrow the static wire id, not allocate a String"
      );
    }
  }

  /// The `"sdk"` provenance (client-published, not a matcher) still reads
  /// as the same wire string a snapshot consumer has always seen — and it
  /// borrows the literal, so saying where a client card came from costs
  /// no allocation.
  #[test]
  fn sdk_provenance_keeps_its_wire_string() {
    // Built exactly the way the SDK publish path builds it
    // (`handle_set_activity` stamps provenance after the payload build).
    let provenance = commands::ActivityProvenance {
      source: Cow::Borrowed("sdk"),
      latency_ms: None,
    };
    assert_eq!(&*provenance.source, "sdk");
    assert!(
      matches!(provenance.source, Cow::Borrowed(_)),
      "the SDK path must borrow the literal, not allocate a String"
    );
  }

  /// F3: a handoff's game name is refcounted, so the resume clone (the
  /// second of four deep `String` copies the chain used to pay) is a
  /// counter bump over the same buffer.
  ///
  /// Pointer identity is the assertion: a copy would land in a fresh
  /// allocation and fail here, while the refcounted field does not.
  #[test]
  fn handoff_resume_shares_the_game_name_buffer() {
    let mut handoff = HandoffState::default();
    let game = ScannedGame {
      id: AppId::from("12345"),
      name: Arc::from("Source Game"),
      pid: 7,
      start: 0,
      source: DetectSource::Automaton,
      process_start_ms: None,
    };
    handoff.note_scan(Some(game));
    let name_ptr = handoff.resume_for("12345").expect("resumed").name.as_ptr();

    let resumed = handoff.resume_for("12345").expect("still resumed");
    assert!(
      std::ptr::eq(name_ptr, resumed.name.as_ptr()),
      "a resumed game must share its name buffer, never copy it"
    );
  }

  /// The replay cache stamps each entry with a fresh sequence number.
  ///
  /// The counter used to live in its own `Mutex` taken *inside* the cache
  /// lock — two acquisitions per publish, and a nested-lock ordering that
  /// exists only because the counter is never touched outside it.
  ///
  /// The invariant the change must not break: every non-clear publish
  /// stamps a sequence the previous one did not, so insertion order stays
  /// recoverable from the cache alone.
  #[tokio::test]
  async fn each_publish_stamps_a_fresh_sequence() {
    let shared = crate::bridge::shared_for_test();
    let mut cmd: ActivityCmd = serde_json::from_str(
      r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
            "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
    )
    .expect("parses");

    shared.handle_set_activity(cmd.clone()).await;
    let first = seq_of(&shared, 1);

    // A different pid, so this is a second cache entry rather than a
    // replace of the first.
    cmd.args.as_mut().unwrap().pid = Some(2);
    shared.handle_set_activity(cmd).await;
    let second = seq_of(&shared, 2);

    assert!(first >= 1, "the first publish stamps a sequence");
    assert!(
      second > first,
      "a second publish must not reuse the first's sequence ({first})"
    );
  }

  /// Only an explicit `activity: null` with a nonzero pid is a genuine
  /// clear; an omitted field is malformed input, not a clear.
  #[test]
  fn genuine_clear_requires_an_explicit_null_activity() {
    let clear: ActivityCmd = serde_json::from_str(
      r#"{"cmd":"SET_ACTIVITY","args":{"pid":1,"activity":null},"nonce":"n"}"#,
    )
    .expect("parses");
    assert!(is_genuine_clear(&clear));

    let missing: ActivityCmd =
      serde_json::from_str(r#"{"cmd":"SET_ACTIVITY","args":{"pid":1},"nonce":"n"}"#)
        .expect("parses");
    assert!(!is_genuine_clear(&missing));

    let zero: ActivityCmd = serde_json::from_str(
      r#"{"cmd":"SET_ACTIVITY","args":{"pid":0,"activity":null},"nonce":"n"}"#,
    )
    .expect("parses");
    assert!(!is_genuine_clear(&zero));
  }

  /// A `SET_ACTIVITY` that omits the `activity` field is ignored: it neither
  /// publishes nor clears the cached card.
  #[tokio::test]
  async fn missing_activity_field_neither_publishes_nor_clears() {
    let shared = crate::bridge::shared_for_test();
    let publish: ActivityCmd = serde_json::from_str(
      r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
            "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
    )
    .expect("parses");
    shared.handle_set_activity(publish).await;
    let before = seq_of(&shared, 1);
    assert!(before >= 1, "the publish must be cached");

    let missing: ActivityCmd = serde_json::from_str(
      r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1},"nonce":"n2"}"#,
    )
    .expect("parses");
    shared.handle_set_activity(missing).await;
    assert_eq!(
      seq_of(&shared, 1),
      before,
      "a missing activity field must not clear the cached card"
    );
  }

  /// A `SET_ACTIVITY` that omits `args` entirely is dropped by the bridge:
  /// it neither publishes nor clears the cached card.
  #[tokio::test]
  async fn absent_args_neither_publishes_nor_clears() {
    let shared = crate::bridge::shared_for_test();
    let publish: ActivityCmd = serde_json::from_str(
      r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
            "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
    )
    .expect("parses");
    shared.handle_set_activity(publish).await;
    let before = seq_of(&shared, 1);
    assert!(before >= 1, "the publish must be cached");

    let absent: ActivityCmd =
      serde_json::from_str(r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","nonce":"n2"}"#)
        .expect("parses");
    shared.handle_set_activity(absent).await;
    assert_eq!(
      seq_of(&shared, 1),
      before,
      "absent args must not change the cached card"
    );
    assert_eq!(
      shared.cache.lock().unwrap_or_else(|e| e.into_inner()).len(),
      1,
      "the publish must stay cached"
    );
  }

  /// A clear evicts rather than stamps, and a republish after it gets a
  /// later sequence than the pre-clear one.
  #[tokio::test]
  async fn a_clear_does_not_reuse_a_sequence() {
    let shared = crate::bridge::shared_for_test();
    let cmd: ActivityCmd = serde_json::from_str(
      r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
            "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
    )
    .expect("parses");
    shared.handle_set_activity(cmd.clone()).await;
    let before = seq_of(&shared, 1);

    let clear = ActivityCmd {
      application_id: Some(adsum_types::AppId::from("app-1")),
      cmd: "SET_ACTIVITY".to_string(),
      data: None,
      evt: None,
      args: Some(adsum_types::cmd::ActivityCmdArgs {
        pid: Some(1),
        activity: Some(None),
        code: None,
        user_id: None,
      }),
      nonce: serde_json::Value::String("n2".to_string()),
    };
    shared.handle_set_activity(clear).await;
    assert!(
      shared
        .cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty(),
      "a clear evicts the entry"
    );

    shared.handle_set_activity(cmd).await;
    assert!(
      seq_of(&shared, 1) > before,
      "a republish must not reuse the pre-clear sequence ({before})"
    );
  }

  /// Sequence stamped for the entry cached under `pid`, or 0 when absent.
  fn seq_of(shared: &crate::bridge::Shared, pid: u64) -> u64 {
    shared
      .cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(&SocketId::from(pid))
      .map(|(_, seq)| *seq)
      .unwrap_or(0)
  }
}
