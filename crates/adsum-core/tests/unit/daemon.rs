use super::*;

/// Each daemon flag disables only its own command class; disabling both
/// is expressed by not binding at all (see `start_scanner`).
#[test]
fn ws_config_maps_each_connector_flag_to_its_class() {
  let cfg = ws_transport_config(&RPCConfig::default());
  assert!(cfg.set_activity);
  assert!(cfg.secondary_events);

  let no_secondary = RPCConfig {
    enable_secondary_events: false,
    ..RPCConfig::default()
  };
  let cfg = ws_transport_config(&no_secondary);
  assert!(cfg.set_activity);
  assert!(!cfg.secondary_events);

  let no_ws = RPCConfig {
    enable_websocket_connector: false,
    ..RPCConfig::default()
  };
  let cfg = ws_transport_config(&no_ws);
  assert!(!cfg.set_activity);
  assert!(cfg.secondary_events);
}

/// `--state-file` routes bridge snapshots to the temp-dir slots; the
/// default keeps snapshots off so ordinary runs never write them.
#[test]
fn state_file_routes_bridge_snapshots_to_tmpdir() {
  let off = bridge_config_for(&RPCConfig::default(), None, None);
  assert!(off.state_dir.is_none());

  let on = bridge_config_for(
    &RPCConfig {
      state_file: true,
      ..RPCConfig::default()
    },
    None,
    None,
  );
  assert_eq!(on.state_dir, Some(std::env::temp_dir()));
}

#[derive(Default)]
struct CancellationBackend {
  started: tokio::sync::Notify,
  exited: std::sync::atomic::AtomicBool,
}

impl adsum_oauth::OAuthBackend for CancellationBackend {
  fn execute(
    &self,
    _: &ActivityCmd,
    _: &str,
    cancellation: &adsum_oauth::OAuthCancellation,
  ) -> std::result::Result<adsum_oauth::RpcOAuthResult, adsum_oauth::OAuthError> {
    self.started.notify_one();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cancellation.is_cancelled() && std::time::Instant::now() < deadline {
      std::thread::sleep(std::time::Duration::from_millis(1));
    }
    self.exited.store(true, std::sync::atomic::Ordering::SeqCst);
    Err(adsum_oauth::OAuthError::Cancelled)
  }
}

fn oauth_test_config() -> RPCConfig {
  RPCConfig {
    port: 0,
    bridge_port_end: 0,
    msgpack_port: 0,
    ws_port_start: 0,
    ws_port_end: 0,
    enable_process_scanner: false,
    enable_proc_events: false,
    enable_db_update: false,
    enable_ipc_connector: false,
    ..RPCConfig::default()
  }
}

async fn pending_oauth() -> (
  Arc<OAuthManager>,
  Arc<CancellationBackend>,
  adsum_oauth::OAuthConnection,
  adsum_oauth::OAuthTask,
) {
  let backend = Arc::new(CancellationBackend::default());
  let manager = Arc::new(OAuthManager::new(
    adsum_oauth::RpcAuthPolicy::Required,
    backend.clone(),
  ));
  let connection = manager.connection(adsum_types::AppId::from("123456789012345678"));
  let command = serde_json::from_value(serde_json::json!({
    "cmd": "AUTHENTICATE", "args": { "access_token": "hermetic-token" }, "nonce": "pending",
  }))
  .unwrap();
  let task = connection.start(command).unwrap();
  tokio::time::timeout(
    std::time::Duration::from_secs(5),
    backend.started.notified(),
  )
  .await
  .expect("OAuth worker started");
  (manager, backend, connection, task)
}

#[tokio::test]
async fn normal_shutdown_joins_daemon_owned_oauth_workers() {
  let (manager, backend, connection, task) = pending_oauth().await;
  let daemon = Daemon::from_parsed(Vec::new(), oauth_test_config());
  tokio::time::timeout(
    std::time::Duration::from_secs(5),
    daemon.run_until_with_oauth(async {}, Arc::clone(&manager)),
  )
  .await
  .expect("daemon shutdown completes")
  .expect("clean shutdown");
  assert!(backend.exited.load(std::sync::atomic::Ordering::SeqCst));
  assert!(matches!(
    connection.session(),
    Err(adsum_oauth::OAuthError::Cancelled)
  ));
  assert!(matches!(
    task.try_result(),
    Some(Err(adsum_oauth::OAuthError::Cancelled))
  ));
}

#[tokio::test]
async fn websocket_bind_failure_still_joins_oauth_workers() {
  let (manager, backend, connection, task) = pending_oauth().await;
  let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
  let port = held.local_addr().unwrap().port();
  let config = RPCConfig {
    ws_port_start: port,
    ws_port_end: port,
    ..oauth_test_config()
  };
  let daemon = Daemon::from_parsed(Vec::new(), config);
  let error = tokio::time::timeout(
    std::time::Duration::from_secs(5),
    daemon.run_until_with_oauth(async {}, Arc::clone(&manager)),
  )
  .await
  .expect("bind failure tears down promptly")
  .expect_err("held WebSocket port must fail");
  assert!(matches!(error, AdsumError::WsBind { .. }));
  assert!(backend.exited.load(std::sync::atomic::Ordering::SeqCst));
  assert!(matches!(
    connection.session(),
    Err(adsum_oauth::OAuthError::Cancelled)
  ));
  assert!(matches!(
    task.try_result(),
    Some(Err(adsum_oauth::OAuthError::Cancelled))
  ));
}

#[tokio::test]
async fn bridge_bind_failure_still_joins_oauth_workers_after_transport_teardown() {
  let (manager, backend, connection, task) = pending_oauth().await;
  let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
  let port = held.local_addr().unwrap().port();
  let config = RPCConfig {
    port,
    bridge_port_end: port,
    ..oauth_test_config()
  };
  let daemon = Daemon::from_parsed(Vec::new(), config);
  tokio::time::timeout(
    std::time::Duration::from_secs(5),
    daemon.run_until_with_oauth(async {}, Arc::clone(&manager)),
  )
  .await
  .expect("bridge bind failure tears down promptly")
  .expect_err("held bridge port must fail");
  assert!(backend.exited.load(std::sync::atomic::Ordering::SeqCst));
  assert!(matches!(
    connection.session(),
    Err(adsum_oauth::OAuthError::Cancelled)
  ));
  assert!(matches!(
    task.try_result(),
    Some(Err(adsum_oauth::OAuthError::Cancelled))
  ));
}

#[tokio::test]
async fn abandoned_run_cancels_oauth_without_waiting_on_the_runtime_thread() {
  let (manager, backend, connection, task) = pending_oauth().await;
  let serving = Arc::new(tokio::sync::Notify::new());
  let notify = Arc::clone(&serving);
  let daemon = Daemon::from_parsed(Vec::new(), oauth_test_config());
  let run = tokio::spawn(daemon.run_until_with_oauth(
    async move {
      notify.notify_one();
      std::future::pending::<()>().await;
    },
    Arc::clone(&manager),
  ));
  tokio::time::timeout(std::time::Duration::from_secs(5), serving.notified())
    .await
    .expect("daemon reached serving state");
  run.abort();
  let _ = run.await;
  assert!(matches!(
    connection.session(),
    Err(adsum_oauth::OAuthError::Cancelled)
  ));
  // Explicitly join in the test: abort only promises cancellation, not a join.
  tokio::task::spawn_blocking(move || manager.shutdown())
    .await
    .unwrap();
  assert!(backend.exited.load(std::sync::atomic::Ordering::SeqCst));
  assert!(matches!(
    task.try_result(),
    Some(Err(adsum_oauth::OAuthError::Cancelled))
  ));
}
