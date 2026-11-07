//! Real WebSocket coverage with instance-local policy and a hermetic OAuth backend.

use std::num::NonZeroUsize;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime};

use adsum_oauth::{
  OAuthBackend, OAuthCancellation, OAuthError, OAuthManager, OAuthSession, RpcAuthPolicy,
  RpcOAuthResult,
};
use adsum_transport_ws::{WsTransport, WsTransportConfig};
use adsum_types::{AppId, cmd::ActivityCmd, user::RpcUser};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(5);
const APP_A: &str = "123456789012345678";
const APP_B: &str = "987654321098765432";
type Socket =
  tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
struct FakeOAuthBackend {
  calls: AtomicUsize,
  active: AtomicUsize,
  peak: AtomicUsize,
  cancelled: AtomicUsize,
  release: AtomicBool,
  expires: Mutex<Option<SystemTime>>,
  expected_clients: Mutex<Vec<String>>,
}

struct ActiveWorker<'a>(&'a FakeOAuthBackend);

impl Drop for ActiveWorker<'_> {
  fn drop(&mut self) {
    self.0.active.fetch_sub(1, Ordering::SeqCst);
  }
}

impl OAuthBackend for FakeOAuthBackend {
  fn execute(
    &self,
    command: &ActivityCmd,
    expected_client_id: &str,
    cancellation: &OAuthCancellation,
  ) -> Result<RpcOAuthResult, OAuthError> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
    self.peak.fetch_max(active, Ordering::SeqCst);
    let _worker = ActiveWorker(self);
    self
      .expected_clients
      .lock()
      .unwrap()
      .push(expected_client_id.to_string());
    let extra = &command
      .args
      .as_ref()
      .ok_or(OAuthError::InvalidAccessToken)?
      .extra;
    let token = extra.get("access_token").and_then(Value::as_str);
    if token.is_some_and(|token| token.starts_with("pending"))
      || extra.get("test_hold").and_then(Value::as_bool) == Some(true)
    {
      while !self.release.load(Ordering::SeqCst) {
        if cancellation.is_cancelled() {
          self.cancelled.fetch_add(1, Ordering::SeqCst);
          // Deliberately return a late success: the manager, not the backend,
          // must prevent a cancelled operation from restoring a session.
          break;
        }
        std::thread::sleep(Duration::from_millis(1));
      }
    }
    if command.cmd == "AUTHORIZE" {
      if extra.get("client_id").and_then(Value::as_str) != Some(expected_client_id) {
        return Err(OAuthError::InvalidClientId);
      }
      return Ok(RpcOAuthResult {
        data: json!({ "code": "hermetic-authorization-code" }),
        session: None,
      });
    }
    let token = token.ok_or(OAuthError::InvalidAccessToken)?;
    let (user_id, username) = match token {
      "alice" | "pending" | "expiring" | "expired" | "no-identify" | "no-rpc" | "other-app" => {
        ("100", "alice")
      }
      "bob" => ("200", "bob"),
      _ => return Err(OAuthError::InvalidAccessToken),
    };
    let application_id = AppId::from(if token == "other-app" { APP_B } else { APP_A });
    let scopes: Vec<String> = match token {
      "no-identify" => vec!["rpc".into()],
      "no-rpc" => vec!["identify".into()],
      _ => vec!["rpc".into(), "identify".into()],
    };
    let user = RpcUser {
      id: user_id.into(),
      username: username.into(),
      ..RpcUser::default()
    };
    let expires = match token {
      "expired" => SystemTime::now() - Duration::from_secs(1),
      "expiring" => SystemTime::now() + Duration::from_millis(750),
      _ => SystemTime::now() + Duration::from_secs(3600),
    };
    if token == "expiring" {
      *self.expires.lock().unwrap() = Some(expires);
    }
    Ok(RpcOAuthResult {
      data: json!({
        "application": { "id": application_id },
        "scopes": scopes,
        "user": (token != "no-identify").then_some(&user),
      }),
      session: Some(Arc::new(OAuthSession {
        application_id,
        // Include a user even without identify to exercise manager sanitization.
        user: Some(user),
        scopes,
        expires,
        access_token: "test-token".to_owned(),
      })),
    })
  }
}

struct TestServer {
  transport: Option<WsTransport>,
  rx: mpsc::Receiver<ActivityCmd>,
  manager: Arc<OAuthManager>,
  backend: Arc<FakeOAuthBackend>,
}

impl TestServer {
  async fn new(policy: RpcAuthPolicy) -> Self {
    Self::with_limit(policy, 4).await
  }

  async fn with_limit(policy: RpcAuthPolicy, limit: usize) -> Self {
    let backend = Arc::new(FakeOAuthBackend::default());
    let manager = Arc::new(if limit == 4 {
      OAuthManager::new(policy, backend.clone())
    } else {
      OAuthManager::with_capacity(policy, backend.clone(), NonZeroUsize::new(limit).unwrap())
    });
    let (transport, rx) = WsTransport::bind_with_oauth(
      WsTransportConfig::new(0, 0),
      legacy_user(),
      Arc::clone(&manager),
    )
    .await
    .unwrap();
    Self {
      transport: Some(transport),
      rx,
      manager,
      backend,
    }
  }

  fn transport(&self) -> &WsTransport {
    self.transport.as_ref().unwrap()
  }

  async fn connect(&self, client_id: Option<&str>) -> Socket {
    connect(self.transport().bound_port(), client_id).await
  }

  async fn wait_for_expiry(&self) {
    let expires = self
      .backend
      .expires
      .lock()
      .unwrap()
      .expect("expiring session");
    wait_until(|| SystemTime::now() >= expires).await;
  }

  async fn shutdown(mut self) {
    if let Some(transport) = self.transport.take() {
      tokio::time::timeout(TIMEOUT, transport.shutdown())
        .await
        .expect("transport shutdown must not wait for blocking OAuth I/O");
    }
    let manager = Arc::clone(&self.manager);
    tokio::task::spawn_blocking(move || manager.shutdown())
      .await
      .unwrap();
    assert_eq!(self.backend.active.load(Ordering::SeqCst), 0);
  }
}

impl Drop for TestServer {
  fn drop(&mut self) {
    self.manager.cancel();
  }
}

fn legacy_user() -> Arc<Mutex<RpcUser>> {
  Arc::new(Mutex::new(RpcUser {
    id: "legacy-user".into(),
    username: "legacy".into(),
    ..RpcUser::default()
  }))
}

async fn wait_until(condition: impl Fn() -> bool) {
  tokio::time::timeout(TIMEOUT, async {
    while !condition() {
      tokio::time::sleep(Duration::from_millis(2)).await;
    }
  })
  .await
  .expect("condition must become true before the deadline");
}

async fn connect(port: u16, client_id: Option<&str>) -> Socket {
  let query = match client_id {
    Some(client_id) => format!("?v=1&encoding=json&client_id={client_id}"),
    None => "?v=1&encoding=json".into(),
  };
  let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{query}"))
    .await
    .unwrap();
  let ready = read_reply(&mut socket).await;
  assert_eq!(ready["evt"], "READY");
  assert_eq!(ready["data"]["user"]["id"], "legacy-user");
  socket
}

async fn read_reply(socket: &mut Socket) -> Value {
  tokio::time::timeout(TIMEOUT, async {
    loop {
      match socket.next().await.expect("socket stays open").unwrap() {
        tungstenite::Message::Text(text) => return serde_json::from_str(&text).unwrap(),
        tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_) => {}
        _ => panic!("expected a JSON text reply"),
      }
    }
  })
  .await
  .expect("RPC reply must arrive before the deadline")
}

async fn send(socket: &mut Socket, command: Value) {
  socket
    .send(tungstenite::Message::Text(command.to_string().into()))
    .await
    .unwrap();
}

async fn rpc(socket: &mut Socket, command: Value) -> Value {
  send(socket, command).await;
  read_reply(socket).await
}

fn auth(token: &str, nonce: &str) -> Value {
  json!({ "cmd": "AUTHENTICATE", "args": { "access_token": token }, "nonce": nonce })
}

fn get_user(nonce: &str) -> Value {
  json!({ "cmd": "GET_USER", "nonce": nonce })
}

fn publish(pid: u64, nonce: &str) -> Value {
  json!({
    "cmd": "SET_ACTIVITY", "args": { "pid": pid, "activity": { "name": "Game", "type": 0 } },
    "nonce": nonce,
  })
}

fn clear(pid: u64, nonce: &str) -> Value {
  json!({ "cmd": "SET_ACTIVITY", "args": { "pid": pid, "activity": null }, "nonce": nonce })
}

fn assert_error(reply: &Value, cmd: &str, nonce: &str, code: u16) {
  assert_eq!(reply["cmd"], cmd);
  assert_eq!(reply["nonce"], nonce);
  assert_eq!(reply["evt"], "ERROR");
  assert_eq!(reply["data"]["code"], code);
}

async fn assert_authenticated(socket: &mut Socket, token: &str) {
  let reply = rpc(socket, auth(token, "auth")).await;
  assert_eq!(reply["cmd"], "AUTHENTICATE");
  assert_eq!(reply["nonce"], "auth");
  assert!(reply["evt"].is_null());
  assert_eq!(reply["data"]["application"]["id"], APP_A);
}

/// Backend exit precedes the manager permit's release; retry only its bounded
/// capacity error while checking that the slot becomes usable again.
async fn assert_capacity_recovers(socket: &mut Socket) {
  tokio::time::timeout(TIMEOUT, async {
    loop {
      let reply = rpc(socket, auth("alice", "recovered")).await;
      if reply["evt"].is_null() {
        assert_eq!(reply["cmd"], "AUTHENTICATE");
        assert_eq!(reply["nonce"], "recovered");
        assert_eq!(reply["data"]["application"]["id"], APP_A);
        break;
      }
      assert_error(&reply, "AUTHENTICATE", "recovered", 5000);
      tokio::time::sleep(Duration::from_millis(2)).await;
    }
  })
  .await
  .expect("cancelled workers must release their capacity");
}

async fn next_sink(server: &mut TestServer) -> ActivityCmd {
  tokio::time::timeout(TIMEOUT, server.rx.recv())
    .await
    .unwrap()
    .expect("sink remains open")
}

fn assert_clear(command: &ActivityCmd, pid: u64) {
  assert_eq!(command.cmd, "SET_ACTIVITY");
  assert_eq!(command.application_id.as_deref(), Some(APP_A));
  let args = command.args.as_ref().unwrap();
  assert_eq!(args.pid, Some(pid));
  assert!(matches!(args.activity, Some(None)));
}

#[tokio::test]
async fn required_policy_gates_commands_but_allows_authorize_then_authenticate() {
  let mut server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut socket = server.connect(Some(APP_A)).await;
  for cmd in [
    "GET_USER",
    "SUBSCRIBE",
    "UNSUBSCRIBE",
    "SET_ACTIVITY",
    "INVITE_BROWSER",
    "GUILD_TEMPLATE_BROWSER",
    "GIFT_CODE_BROWSER",
    "DEEP_LINK",
    "CONNECTIONS_CALLBACK",
    "UNKNOWN",
  ] {
    let reply = rpc(
      &mut socket,
      json!({ "cmd": cmd, "args": { "pid": 1, "activity": null, "code": "invite" }, "nonce": cmd }),
    )
    .await;
    assert_error(&reply, cmd, cmd, 4006);
  }
  assert!(server.rx.try_recv().is_err());
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 0);

  let reply = rpc(
    &mut socket,
    json!({
      "cmd": "AUTHORIZE", "args": { "client_id": APP_A, "scopes": ["rpc", "identify"] },
      "nonce": "authorize",
    }),
  )
  .await;
  assert_eq!(reply["cmd"], "AUTHORIZE");
  assert_eq!(reply["nonce"], "authorize");
  assert_eq!(reply["data"]["code"], "hermetic-authorization-code");
  assert_error(
    &rpc(&mut socket, get_user("still-gated")).await,
    "GET_USER",
    "still-gated",
    4006,
  );

  assert_authenticated(&mut socket, "alice").await;
  let reply = rpc(&mut socket, get_user("identity")).await;
  assert_eq!(reply["data"]["id"], "100");
  assert_eq!(reply["data"]["username"], "alice");
  let reply = rpc(&mut socket, publish(1, "presence")).await;
  assert_eq!(reply["nonce"], "presence");
  assert_eq!(
    next_sink(&mut server).await.application_id.as_deref(),
    Some(APP_A)
  );
  server.shutdown().await;
}

#[tokio::test]
async fn oauth_requires_a_valid_query_client_and_cannot_replace_it_with_command_ids() {
  let server = TestServer::new(RpcAuthPolicy::Required).await;
  for client_id in [
    None,
    Some(""),
    Some("not-an-id"),
    Some("123456789012345678901"),
  ] {
    let mut socket = server.connect(client_id).await;
    assert_error(
      &rpc(&mut socket, auth("alice", "missing-query")).await,
      "AUTHENTICATE",
      "missing-query",
      4007,
    );
    assert_error(
      &rpc(
        &mut socket,
        json!({
          "cmd": "AUTHORIZE", "application_id": APP_A,
          "args": { "client_id": APP_A, "scopes": ["rpc"] }, "nonce": "cannot-override",
        }),
      )
      .await,
      "AUTHORIZE",
      "cannot-override",
      4007,
    );
    socket.close(None).await.unwrap();
  }
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 0);

  let mut socket = server.connect(Some(APP_A)).await;
  assert_error(
    &rpc(
      &mut socket,
      json!({
        "cmd": "AUTHENTICATE", "application_id": APP_B,
        "args": { "access_token": "alice" }, "nonce": "different-command-app",
      }),
    )
    .await,
    "AUTHENTICATE",
    "different-command-app",
    4007,
  );
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 0);
  assert_error(&rpc(&mut socket, json!({
    "cmd": "AUTHORIZE", "args": { "client_id": APP_B, "scopes": ["rpc"] }, "nonce": "different-args-app",
  })).await, "AUTHORIZE", "different-args-app", 4007);
  assert_eq!(
    server.backend.expected_clients.lock().unwrap().as_slice(),
    [APP_A]
  );

  let mut wrong_query = server.connect(Some(APP_B)).await;
  assert_error(
    &rpc(&mut wrong_query, auth("alice", "wrong-query")).await,
    "AUTHENTICATE",
    "wrong-query",
    4007,
  );
  assert_error(
    &rpc(&mut wrong_query, get_user("no-session")).await,
    "GET_USER",
    "no-session",
    4006,
  );
  assert_authenticated(&mut socket, "alice").await;
  server.shutdown().await;
}

#[tokio::test]
async fn invalid_tokens_scopes_expiry_and_cross_app_sessions_never_authenticate() {
  let server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut socket = server.connect(Some(APP_A)).await;
  for (token, code) in [
    ("invalid", 4009),
    ("no-rpc", 4006),
    ("expired", 4009),
    ("other-app", 4007),
  ] {
    assert_error(
      &rpc(&mut socket, auth(token, token)).await,
      "AUTHENTICATE",
      token,
      code,
    );
    assert_error(
      &rpc(&mut socket, get_user("no-session")).await,
      "GET_USER",
      "no-session",
      4006,
    );
  }
  assert_authenticated(&mut socket, "alice").await;
  assert_error(
    &rpc(&mut socket, auth("invalid", "reauth")).await,
    "AUTHENTICATE",
    "reauth",
    4009,
  );
  assert_error(
    &rpc(&mut socket, get_user("no-fallback")).await,
    "GET_USER",
    "no-fallback",
    4006,
  );
  server.shutdown().await;
}

#[tokio::test]
async fn get_user_requires_identify_and_only_returns_the_authenticated_user() {
  let server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut socket = server.connect(Some(APP_A)).await;
  assert_authenticated(&mut socket, "no-identify").await;
  let reply = rpc(&mut socket, get_user("no-identify")).await;
  assert!(reply["data"].is_null());
  assert!(reply["evt"].is_null());

  assert_authenticated(&mut socket, "alice").await;
  for (wanted, found) in [("100", true), ("200", false), ("legacy-user", false)] {
    let reply = rpc(
      &mut socket,
      json!({
        "cmd": "GET_USER", "args": { "user_id": wanted }, "nonce": wanted,
      }),
    )
    .await;
    assert_eq!(!reply["data"].is_null(), found);
  }
  server.shutdown().await;
}

#[tokio::test]
async fn expired_sessions_fail_the_discovering_command_then_require_authentication() {
  let mut server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut socket = server.connect(Some(APP_A)).await;
  assert_authenticated(&mut socket, "expiring").await;
  server.wait_for_expiry().await;
  assert_error(
    &rpc(&mut socket, get_user("expired")).await,
    "GET_USER",
    "expired",
    4009,
  );
  assert_error(
    &rpc(&mut socket, get_user("no-fallback")).await,
    "GET_USER",
    "no-fallback",
    4006,
  );
  assert_error(
    &rpc(&mut socket, publish(10, "gated")).await,
    "SET_ACTIVITY",
    "gated",
    4006,
  );
  assert!(server.rx.try_recv().is_err());
  server.shutdown().await;
}

#[tokio::test]
async fn owned_clears_survive_expiry_and_failed_reauth_but_cannot_clear_other_pids() {
  let mut server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut owner = server.connect(Some(APP_A)).await;
  let mut other = server.connect(Some(APP_A)).await;
  assert_authenticated(&mut owner, "expiring").await;
  for pid in [11, 22] {
    let reply = rpc(&mut owner, publish(pid, "published")).await;
    assert!(reply["evt"].is_null());
    let _ = next_sink(&mut server).await;
  }
  assert_error(
    &rpc(&mut other, clear(22, "not-owned")).await,
    "SET_ACTIVITY",
    "not-owned",
    4006,
  );
  server.wait_for_expiry().await;
  let reply = rpc(&mut owner, clear(11, "expired-owned")).await;
  assert!(reply["evt"].is_null());
  assert_clear(&next_sink(&mut server).await, 11);
  assert_error(
    &rpc(&mut owner, clear(99, "untracked")).await,
    "SET_ACTIVITY",
    "untracked",
    4006,
  );
  assert_error(
    &rpc(&mut owner, auth("invalid", "failed-reauth")).await,
    "AUTHENTICATE",
    "failed-reauth",
    4009,
  );
  assert_error(
    &rpc(
      &mut owner,
      json!({
        "cmd": "SET_ACTIVITY", "application_id": APP_B,
        "args": { "pid": 22, "activity": null }, "nonce": "other-app",
      }),
    )
    .await,
    "SET_ACTIVITY",
    "other-app",
    4007,
  );
  let reply = rpc(&mut owner, clear(22, "failed-auth-owned")).await;
  assert!(reply["evt"].is_null());
  assert_clear(&next_sink(&mut server).await, 22);
  assert_error(
    &rpc(&mut owner, clear(22, "already-cleared")).await,
    "SET_ACTIVITY",
    "already-cleared",
    4006,
  );
  assert_error(
    &rpc(
      &mut owner,
      json!({
        "cmd": "SET_ACTIVITY", "args": { "activity": null }, "nonce": "no-pid",
      }),
    )
    .await,
    "SET_ACTIVITY",
    "no-pid",
    4006,
  );
  assert!(server.rx.try_recv().is_err());
  server.shutdown().await;
}

#[tokio::test]
async fn two_connections_never_share_identity_or_failed_authentication() {
  let server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut alice = server.connect(Some(APP_A)).await;
  let mut bob = server.connect(Some(APP_A)).await;
  assert_authenticated(&mut alice, "alice").await;
  assert_error(
    &rpc(&mut bob, get_user("isolated")).await,
    "GET_USER",
    "isolated",
    4006,
  );
  assert_authenticated(&mut bob, "bob").await;
  assert_eq!(
    rpc(&mut alice, get_user("alice")).await["data"]["username"],
    "alice"
  );
  assert_eq!(
    rpc(&mut bob, get_user("bob")).await["data"]["username"],
    "bob"
  );
  assert_error(
    &rpc(&mut alice, auth("invalid", "fail")).await,
    "AUTHENTICATE",
    "fail",
    4009,
  );
  assert_error(
    &rpc(&mut alice, get_user("forgotten")).await,
    "GET_USER",
    "forgotten",
    4006,
  );
  assert_eq!(
    rpc(&mut bob, get_user("unaffected")).await["data"]["username"],
    "bob"
  );
  server.shutdown().await;
}

#[tokio::test]
async fn concurrent_oauth_keeps_one_mailbox_and_rejected_auth_cannot_erase_a_session() {
  let server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut socket = server.connect(Some(APP_A)).await;
  assert_authenticated(&mut socket, "alice").await;
  send(
    &mut socket,
    json!({
      "cmd": "AUTHORIZE", "args": { "client_id": APP_A, "scopes": ["rpc"], "test_hold": true },
      "nonce": "pending-authorize",
    }),
  )
  .await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 1).await;
  for n in 0..32 {
    let nonce = format!("busy-{n}");
    assert_error(
      &rpc(&mut socket, auth("invalid", &nonce)).await,
      "AUTHENTICATE",
      &nonce,
      5000,
    );
  }
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 2);
  assert_eq!(
    rpc(&mut socket, get_user("still-alice")).await["data"]["username"],
    "alice"
  );
  server.backend.release.store(true, Ordering::SeqCst);
  let reply = read_reply(&mut socket).await;
  assert_eq!(reply["cmd"], "AUTHORIZE");
  assert_eq!(reply["nonce"], "pending-authorize");

  server.backend.release.store(false, Ordering::SeqCst);
  send(&mut socket, auth("pending", "pending-auth")).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 1).await;
  assert_error(
    &rpc(&mut socket, auth("invalid", "concurrent-auth")).await,
    "AUTHENTICATE",
    "concurrent-auth",
    5000,
  );
  server.backend.release.store(true, Ordering::SeqCst);
  let reply = read_reply(&mut socket).await;
  assert_eq!(reply["cmd"], "AUTHENTICATE");
  assert_eq!(reply["nonce"], "pending-auth");
  assert_eq!(
    rpc(&mut socket, get_user("committed-once")).await["data"]["username"],
    "alice"
  );
  assert_eq!(server.backend.peak.load(Ordering::SeqCst), 1);
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 3);
  server.shutdown().await;
}

#[tokio::test]
async fn default_worker_budget_is_four_and_full_capacity_never_blocks_the_pump() {
  let server = TestServer::new(RpcAuthPolicy::Required).await;
  let mut pending = Vec::new();
  for _ in 0..4 {
    let mut socket = server.connect(Some(APP_A)).await;
    send(&mut socket, auth("pending", "held")).await;
    pending.push(socket);
  }
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 4).await;
  let mut overflow = server.connect(Some(APP_A)).await;
  assert_error(
    &rpc(&mut overflow, auth("alice", "at-capacity")).await,
    "AUTHENTICATE",
    "at-capacity",
    5000,
  );
  assert_error(
    &rpc(&mut overflow, get_user("pump-alive")).await,
    "GET_USER",
    "pump-alive",
    4006,
  );
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 4);
  assert_eq!(server.backend.peak.load(Ordering::SeqCst), 4);

  drop(pending.pop());
  wait_until(|| server.backend.cancelled.load(Ordering::SeqCst) == 1).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 3).await;
  assert_capacity_recovers(&mut overflow).await;
  server.backend.release.store(true, Ordering::SeqCst);
  for socket in &mut pending {
    let reply = read_reply(socket).await;
    assert_eq!(reply["cmd"], "AUTHENTICATE");
    assert_eq!(reply["nonce"], "held");
    assert!(reply["evt"].is_null());
  }
  assert_eq!(server.backend.peak.load(Ordering::SeqCst), 4);
  server.shutdown().await;
}

#[tokio::test]
async fn disconnect_cancels_pending_auth_and_clears_presence_before_reconnection() {
  let mut server = TestServer::with_limit(RpcAuthPolicy::Required, 1).await;
  let mut socket = server.connect(Some(APP_A)).await;
  assert_authenticated(&mut socket, "alice").await;
  let _ = rpc(&mut socket, publish(77, "published")).await;
  let _ = next_sink(&mut server).await;
  send(&mut socket, auth("pending", "abandoned")).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 1).await;
  drop(socket);
  assert_clear(&next_sink(&mut server).await, 77);
  wait_until(|| server.backend.cancelled.load(Ordering::SeqCst) == 1).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 0).await;
  wait_until(|| server.transport().client_total().load(Ordering::Relaxed) == 0).await;
  let mut replacement = server.connect(Some(APP_A)).await;
  assert_error(
    &rpc(&mut replacement, get_user("no-late-identity")).await,
    "GET_USER",
    "no-late-identity",
    4006,
  );
  assert_authenticated(&mut replacement, "alice").await;
  server.shutdown().await;
}

#[tokio::test]
async fn shared_manager_budget_and_shutdown_ownership_span_transport_instances() {
  let mut server = TestServer::with_limit(RpcAuthPolicy::Required, 1).await;
  let (other, _rx) = WsTransport::bind_with_oauth(
    WsTransportConfig::new(0, 0),
    legacy_user(),
    Arc::clone(&server.manager),
  )
  .await
  .unwrap();
  let mut first = server.connect(Some(APP_A)).await;
  let mut second = connect(other.bound_port(), Some(APP_A)).await;
  send(&mut first, auth("pending", "held")).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 1).await;
  assert_error(
    &rpc(&mut second, auth("alice", "shared-capacity")).await,
    "AUTHENTICATE",
    "shared-capacity",
    5000,
  );
  tokio::time::timeout(TIMEOUT, server.transport.take().unwrap().shutdown())
    .await
    .unwrap();
  wait_until(|| server.backend.cancelled.load(Ordering::SeqCst) == 1).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 0).await;
  // Closing the first transport cancels its connection, not the shared manager.
  assert_capacity_recovers(&mut second).await;
  other.shutdown().await;
  server.shutdown().await;
}

#[tokio::test]
async fn drop_cancels_pending_work_even_when_a_telemetry_handle_retains_the_slots() {
  let mut server = TestServer::with_limit(RpcAuthPolicy::Required, 1).await;
  let mut socket = server.connect(Some(APP_A)).await;
  let handle = server.transport().handle();
  send(&mut socket, auth("pending", "held")).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 1).await;
  drop(server.transport.take());
  wait_until(|| server.backend.cancelled.load(Ordering::SeqCst) == 1).await;
  wait_until(|| server.backend.active.load(Ordering::SeqCst) == 0).await;
  let (other, _rx) = WsTransport::bind_with_oauth(
    WsTransportConfig::new(0, 0),
    legacy_user(),
    Arc::clone(&server.manager),
  )
  .await
  .unwrap();
  let mut replacement = connect(other.bound_port(), Some(APP_A)).await;
  assert_authenticated(&mut replacement, "alice").await;
  drop(handle);
  other.shutdown().await;
  server.shutdown().await;
}

#[tokio::test]
async fn authenticated_permissive_connections_also_reject_application_overrides() {
  let mut server = TestServer::new(RpcAuthPolicy::Permissive).await;
  let mut socket = server.connect(Some(APP_A)).await;
  assert_authenticated(&mut socket, "alice").await;
  for mut command in [
    get_user("wrong-user-app"),
    publish(5, "wrong-presence-app"),
    auth("alice", "wrong-auth-app"),
  ] {
    command["application_id"] = json!(APP_B);
    let cmd = command["cmd"].as_str().unwrap().to_string();
    let nonce = command["nonce"].as_str().unwrap().to_string();
    assert_error(&rpc(&mut socket, command).await, &cmd, &nonce, 4007);
  }
  assert!(server.rx.try_recv().is_err());
  assert_eq!(
    rpc(&mut socket, get_user("identity-intact")).await["data"]["username"],
    "alice"
  );
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 1);
  server.shutdown().await;
}

#[tokio::test]
async fn permissive_legacy_presence_and_cosmetic_identity_remain_unchanged() {
  let mut server = TestServer::new(RpcAuthPolicy::Permissive).await;
  let mut socket = server.connect(Some("legacy-app")).await;
  assert_eq!(
    rpc(&mut socket, get_user("legacy-user")).await["data"]["username"],
    "legacy"
  );
  let reply = rpc(&mut socket, publish(1, "legacy-presence")).await;
  assert!(reply["evt"].is_null());
  assert_eq!(
    next_sink(&mut server).await.application_id.as_deref(),
    Some("legacy-app")
  );
  let mut command = publish(2, "legacy-override");
  command["application_id"] = json!("legacy-override-app");
  let reply = rpc(&mut socket, command).await;
  assert!(reply["evt"].is_null());
  assert_eq!(
    next_sink(&mut server).await.application_id.as_deref(),
    Some("legacy-override-app")
  );
  assert_eq!(server.backend.calls.load(Ordering::SeqCst), 0);
  server.shutdown().await;
}
