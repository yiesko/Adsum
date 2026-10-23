use super::*;
use crate::handoff::ScannedGame;
use adsum_types::DetectSource;

/// Registration means presence in the protocol table: refused origins
/// never insert, so their ids stay unknown even if a frame arrives.
/// (Guards the `is_registered` gate in the message pump.)
#[test]
fn registration_means_protocol_table_presence() {
  let shared = Shared {
    consumers: Mutex::new(FxHashMap::default()),
    cache: Mutex::new(HashMap::new()),
    activity_seq: std::sync::atomic::AtomicU64::new(0),
    last_process: Mutex::new(HashMap::new()),
    handoff: Mutex::new(HandoffState::default()),
    recent: Mutex::new(RecentActivities::default()),
    user: Arc::new(Mutex::new(RpcUser::default())),
    dirty: AtomicBool::new(false),
    dropped_broadcasts: AtomicU64::new(0),
    app_version: String::new(),
    state_path: None,
    json_port: 0,
    msgpack_port: 0,
    ws_port: None,
    ipc_path: None,
    ipc_tx: None,
    game_tx: None,
    proc_tx: None,
    game_clients: None,
    watch_shed: None,
    allowed_origins: Vec::new(),
  };
  assert!(!shared.is_registered(7));
  // A responder over a dropped receiver: registration is presence in the
  // one consumer map, so an unknown id answers false even if a frame for
  // it ever arrives (the `is_registered` gate in the message pump).
  shared.consumers.lock().unwrap().insert(
    7,
    Consumer {
      responder: adsum_ws::Responder::for_test(),
      protocol: BridgeProtocol::Json,
    },
  );
  assert!(shared.is_registered(7));
  assert!(!shared.is_registered(8));
  // Protocol resolution reads the same map.
  assert_eq!(
    shared.protocol_for(7, BridgeProtocol::MsgPack),
    BridgeProtocol::Json
  );
  assert_eq!(
    shared.protocol_for(8, BridgeProtocol::MsgPack),
    BridgeProtocol::MsgPack
  );
}

/// Publishing past the replay bound keeps the cache within it: steady
/// memory no matter how many distinct publishers arrive (same
/// regression net as `prune_keeps_cache_within_bound`, through the
/// real broadcast path).
#[test]
fn broadcast_keeps_replay_cache_bounded() {
  let shared = Shared {
    consumers: Mutex::new(FxHashMap::default()),
    cache: Mutex::new(HashMap::new()),
    activity_seq: std::sync::atomic::AtomicU64::new(0),
    last_process: Mutex::new(HashMap::new()),
    handoff: Mutex::new(HandoffState::default()),
    recent: Mutex::new(RecentActivities::default()),
    user: Arc::new(Mutex::new(RpcUser::default())),
    dirty: AtomicBool::new(false),
    dropped_broadcasts: AtomicU64::new(0),
    app_version: String::new(),
    state_path: None,
    json_port: 0,
    msgpack_port: 0,
    ws_port: None,
    ipc_path: None,
    ipc_tx: None,
    game_tx: None,
    proc_tx: None,
    game_clients: None,
    watch_shed: None,
    allowed_origins: Vec::new(),
  };
  for i in 0..(crate::config::MAX_CACHED_ACTIVITIES * 2) {
    let cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
      "cmd": "SET_ACTIVITY",
      "application_id": format!("app-{i}"),
      "args": {"pid": i, "activity": {"name": "G", "type": 0}},
      "nonce": "n",
    }))
    .expect("test command builds");
    let payload = adsum_protocol::commands::cached_activity(cmd, None).expect("fixed shapes build");
    shared.broadcast_activity(payload, SocketId::from(i.to_string()));
  }
  let len = shared.cache.lock().unwrap_or_else(|e| e.into_inner()).len();
  assert!(
    len <= crate::config::MAX_CACHED_ACTIVITIES,
    "cache must stay bounded, got {len}"
  );
}

/// Non-`SET_ACTIVITY` fan-out keeps the legacy shape: an explicit null
/// activity is dropped, and a real activity is left untouched.
#[test]
fn raw_fanout_drops_an_explicit_null_activity() {
  let mut cmd: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"INVITE_BROWSER","args":{"pid":3,"activity":null,"code":"abc"},"nonce":"n"}"#,
  )
  .expect("test command builds");
  strip_explicit_null_activity(&mut cmd);
  let args = cmd.args.as_ref().expect("args stay");
  assert!(args.activity.is_none(), "an explicit null must be dropped");
  assert_eq!(args.code.as_deref(), Some("abc"));

  let mut publish: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"INVITE_BROWSER","args":{"pid":3,"activity":{"name":"G","type":0}},"nonce":"n"}"#,
  )
  .expect("test command builds");
  strip_explicit_null_activity(&mut publish);
  assert!(
    publish
      .args
      .as_ref()
      .and_then(|args| args.activity.as_ref())
      .is_some_and(|activity| activity.is_some()),
    "a real activity must survive"
  );
}

/// Session lines fire only on empty<->non-empty edges; slot churn stays quiet.
#[test]
fn table_transition_fires_only_on_state_edges() {
  let game = ScannedGame {
    id: AppId::from("1"),
    name: Arc::from("G"),
    pid: 7,
    start: 0,
    source: DetectSource::Automaton,
    process_start_ms: None,
  };
  // Empty -> game: session start; repeats stay quiet.
  assert_eq!(
    table_transition(false, &ProcInput::Detected(game.clone())),
    Some("game-start")
  );
  assert_eq!(table_transition(true, &ProcInput::Detected(game)), None);
  // Stale removals (pid mismatch) release nothing; matching ones do.
  let mut table = HashMap::new();
  table.insert(AppId::from("1"), 7u64);
  assert_eq!(
    take_matching_process(&mut table, &AppId::from("1"), 8),
    None
  );
  assert!(table.contains_key(&AppId::from("1")));
  assert_eq!(
    take_matching_process(&mut table, &AppId::from("1"), 7),
    Some(7)
  );
  assert!(!table.contains_key(&AppId::from("1")));
  assert_eq!(
    take_matching_process(&mut table, &AppId::from("9"), 7),
    None
  );
  // Game -> empty: session end; repeats stay quiet.
  assert_eq!(
    table_transition(true, &ProcInput::Cleared),
    Some("game-end")
  );
  assert_eq!(table_transition(false, &ProcInput::Cleared), None);
  // Per-slot churn stays quiet in both occupancy states.
  assert_eq!(
    table_transition(true, &ProcInput::Removed(AppId::from("1"), 7)),
    None
  );
  assert_eq!(
    table_transition(false, &ProcInput::Removed(AppId::from("1"), 7)),
    None
  );
}

/// The periodic refresh must not do work nobody asked for.
///
/// It used to clone the whole replay cache into a Vec under the cache
/// lock before checking whether anyone was listening, and `send_to_all`
/// returns immediately with no consumers — so every interval paid a Vec
/// allocation plus one refcount bump per cached card to send nothing.
///
/// The snapshot timestamp still has to move: a live-but-idle daemon must
/// not look stale to slot reuse, which is exactly the invariant an early
/// return is most likely to break.
#[test]
fn refresh_with_no_consumers_still_touches_the_snapshot() {
  let shared = Arc::new(Shared {
    consumers: Mutex::new(FxHashMap::default()),
    cache: Mutex::new(HashMap::new()),
    activity_seq: std::sync::atomic::AtomicU64::new(0),
    last_process: Mutex::new(HashMap::new()),
    handoff: Mutex::new(HandoffState::default()),
    recent: Mutex::new(RecentActivities::default()),
    user: Arc::new(Mutex::new(RpcUser::default())),
    dirty: AtomicBool::new(false),
    dropped_broadcasts: AtomicU64::new(0),
    app_version: String::new(),
    state_path: None,
    json_port: 0,
    msgpack_port: 0,
    ws_port: None,
    ipc_path: None,
    ipc_tx: None,
    game_tx: None,
    proc_tx: None,
    game_clients: None,
    watch_shed: None,
    allowed_origins: Vec::new(),
  });
  // One cached card, so the old path had something to clone.
  let payload = commands::empty_cached(1, SocketId::from("sock-1")).expect("payload");
  shared
    .cache
    .lock()
    .unwrap()
    .insert(SocketId::from("sock-1"), (payload, 0));
  assert!(!shared.dirty.load(Ordering::Relaxed));

  refresh_once(&shared);

  assert!(
    shared.dirty.load(Ordering::Relaxed),
    "an idle refresh must still move the snapshot timestamp"
  );
  // Nothing was broadcast: there is no consumer to send to.
  assert!(
    shared.consumers.lock().unwrap().is_empty(),
    "no consumer should be registered by a refresh"
  );
}
