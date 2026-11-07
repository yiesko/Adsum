use super::*;

struct ImmediateBackend;

impl adsum_oauth::OAuthBackend for ImmediateBackend {
  fn execute(
    &self,
    _: &ActivityCmd,
    expected_client_id: &str,
    _: &adsum_oauth::OAuthCancellation,
  ) -> std::result::Result<adsum_oauth::RpcOAuthResult, OAuthError> {
    Ok(adsum_oauth::RpcOAuthResult {
      data: serde_json::json!({ "authenticated": true }),
      session: Some(std::sync::Arc::new(adsum_oauth::OAuthSession {
        application_id: AppId::from(expected_client_id),
        user: Some(RpcUser::default()),
        scopes: vec!["rpc".into(), "identify".into()],
        expires: std::time::SystemTime::now() + Duration::from_secs(3600),
        access_token: "test-token".to_owned(),
      })),
    })
  }
}

/// Exercise the OAuth send-failure arm itself, not a Disconnect racing it:
/// a real socket stops reading while the responder's bounded outbox fills.
#[tokio::test]
async fn failed_oauth_reply_closes_the_session_and_clears_every_publication() {
  let config = adsum_ws::ServerConfig::builder("127.0.0.1:0".parse().unwrap())
    .per_client_queue(1)
    .build()
    .unwrap();
  let (server, mut hub) = adsum_ws::Server::bind(config).await.unwrap();
  let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{}/", server.local_addr()))
    .await
    .unwrap();
  let Event::Connect(id, responder) = hub.next_event().await.unwrap() else {
    panic!("first event must register the socket");
  };
  let big = Message::Text("x".repeat(8 * 1024 * 1024).into());
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      match responder.try_send(big.clone()) {
        Ok(()) => tokio::time::sleep(Duration::from_millis(5)).await,
        Err(adsum_ws::TrySendError::Full) => break,
        Err(_) => panic!("the socket must remain connected while filling its outbox"),
      }
    }
  })
  .await
  .expect("unread socket must backpressure the bounded outbox");

  let manager = Arc::new(OAuthManager::new(
    adsum_oauth::RpcAuthPolicy::Required,
    Arc::new(ImmediateBackend),
  ));
  let app_id = AppId::from("123456789012345678");
  let connection = manager.connection(app_id.clone());
  let event: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "AUTHENTICATE", "args": { "access_token": "hermetic-token" }, "nonce": "auth",
  }))
  .unwrap();
  let task = connection.start(event).unwrap();
  let clients = Arc::new(RwLock::new(FxHashMap::from_iter([(
    id,
    ClientSlot {
      responder,
      published: [41, 42]
        .into_iter()
        .map(|pid| PublishedSlot {
          app_id: Some(app_id.clone()),
          pid,
          nonce: Value::from("published"),
        })
        .collect(),
      query_client_id: Some(app_id),
      oauth: connection.clone(),
      oauth_reply: Some(PendingOAuthReply {
        event: ActivityCmd {
          cmd: "AUTHENTICATE".into(),
          nonce: Value::from("auth"),
          ..ActivityCmd::empty()
        },
        task: Mutex::new(task),
      }),
    },
  )])));
  let total = Arc::new(AtomicU64::new(1));
  let (tx, mut rx) = mpsc::channel(4);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Arc::new(Mutex::new(PendingQueue::new()));
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      poll_oauth_replies(&clients, &total, &sink, &pending).await;
      if clients.read().await.is_empty() {
        break;
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .expect("a failed OAuth reply must prune the slot instead of parking the pump");
  assert_eq!(total.load(Ordering::Relaxed), 0);
  assert!(matches!(connection.session(), Err(OAuthError::Cancelled)));
  for pid in [41, 42] {
    let clear = rx.try_recv().expect("each published pid must be cleared");
    let args = clear.args.unwrap();
    assert_eq!(args.pid, Some(pid));
    assert!(matches!(args.activity, Some(None)));
  }
  assert!(rx.try_recv().is_err());
  drop(socket);
  server.shutdown().await;
  tokio::task::spawn_blocking(move || manager.shutdown())
    .await
    .unwrap();
}
