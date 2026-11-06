//! Hermetic RPC OAuth flows through the real, nonblocking IPC pump.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use adsum_oauth::{
  OAuthBackend, OAuthCancellation, OAuthError, OAuthManager, OAuthSession, RpcAuthPolicy,
  RpcOAuthResult,
};
use adsum_transport_ipc::frame::{IpcFacilitator, PacketType, admit_pid, encode, handle_stream};
use adsum_transport_ipc::{EventSink, IpcTransport};
use adsum_types::AppId;
use adsum_types::cmd::ActivityCmd;
use adsum_types::user::RpcUser;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use common::{Scratch, read_frame, recv_cmd, write_frame};

const APP: &str = "123456789012345678";
const OTHER_APP: &str = "987654321098765432";
const USER: &str = "111111111111111111";
const WAIT: Duration = Duration::from_secs(3);

struct Request {
  command: String,
  client_id: String,
  cancellation: OAuthCancellation,
  reply: mpsc::SyncSender<Result<RpcOAuthResult, OAuthError>>,
}

impl Request {
  fn finish(&self, result: Result<RpcOAuthResult, OAuthError>) {
    self
      .reply
      .send(result)
      .expect("worker waiting for fake result");
  }

  fn assert_cancelled(&self) {
    wait_until(|| self.cancellation.is_cancelled());
  }
}

struct Backend {
  requests: mpsc::Sender<Request>,
  ignore_cancellation: bool,
}

impl OAuthBackend for Backend {
  fn execute(
    &self,
    command: &ActivityCmd,
    client_id: &str,
    cancellation: &OAuthCancellation,
  ) -> Result<RpcOAuthResult, OAuthError> {
    let (reply, result) = mpsc::sync_channel(1);
    self
      .requests
      .send(Request {
        command: command.cmd.to_string(),
        client_id: client_id.to_owned(),
        cancellation: cancellation.clone(),
        reply,
      })
      .map_err(|_| OAuthError::WorkerUnavailable)?;
    let deadline = Instant::now() + WAIT;
    loop {
      if !self.ignore_cancellation {
        cancellation.check()?;
      }
      match result.recv_timeout(Duration::from_millis(5)) {
        Ok(result) => return result,
        Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
        Err(_) => return Err(OAuthError::WorkerUnavailable),
      }
    }
  }
}

struct OAuthFixture {
  manager: Arc<OAuthManager>,
  requests: mpsc::Receiver<Request>,
}

impl OAuthFixture {
  fn new(policy: RpcAuthPolicy) -> Self {
    Self::with_backend(policy, false)
  }

  fn with_backend(policy: RpcAuthPolicy, ignore_cancellation: bool) -> Self {
    let (tx, requests) = mpsc::channel();
    let manager = Arc::new(OAuthManager::new(
      policy,
      Arc::new(Backend {
        requests: tx,
        ignore_cancellation,
      }),
    ));
    Self { manager, requests }
  }

  fn request(&self, command: &str, app: &str) -> Request {
    let request = self
      .requests
      .recv_timeout(WAIT)
      .expect("OAuth worker started");
    assert_eq!(request.command, command);
    assert_eq!(request.client_id, app);
    request
  }

  fn authenticate(&self, peer: &mut Peer, identify: bool) {
    peer.send(authenticate("auth"));
    let request = self.request("AUTHENTICATE", APP);
    request.finish(Ok(session(
      APP,
      identify,
      SystemTime::now() + Duration::from_secs(60),
    )));
    let reply = peer.reply();
    assert_success(&reply, "auth");
  }
}

impl Drop for OAuthFixture {
  fn drop(&mut self) {
    self.manager.shutdown();
  }
}

struct Facilitator {
  handshake: bool,
  app: AppId,
  pid: u64,
  nonce: Value,
  oauth: Arc<OAuthManager>,
  token: CancellationToken,
  sink: EventSink,
  pids: Vec<u64>,
}

impl IpcFacilitator for Facilitator {
  fn handshake(&self) -> bool {
    self.handshake
  }
  fn set_handshake(&mut self, value: bool) {
    self.handshake = value;
  }
  fn client_id(&self) -> AppId {
    self.app.clone()
  }
  fn set_client_id(&mut self, value: AppId) {
    self.app = value;
  }
  fn pid(&self) -> u64 {
    self.pid
  }
  fn set_pid(&mut self, value: u64) {
    self.pid = value;
  }
  fn nonce(&self) -> Value {
    self.nonce.clone()
  }
  fn set_nonce(&mut self, value: Value) {
    self.nonce = value;
  }
  fn user_payload(&self) -> String {
    RpcUser::default().ready_payload()
  }
  fn current_user(&self) -> RpcUser {
    RpcUser::default()
  }
  fn oauth_manager(&self) -> &Arc<OAuthManager> {
    &self.oauth
  }
  fn is_cancelled(&self) -> bool {
    self.token.is_cancelled()
  }
  fn sink(&self) -> &EventSink {
    &self.sink
  }
  fn admit_published_pid(&mut self, pid: u64) -> bool {
    admit_pid(&mut self.pids, pid)
  }
  fn owns_published_pid(&self, pid: u64) -> bool {
    self.pids.contains(&pid)
  }
  fn forget_published_pid(&mut self, pid: u64) {
    self.pids.retain(|known| *known != pid);
  }
  fn take_published_pids(&mut self) -> Vec<u64> {
    std::mem::take(&mut self.pids)
  }
}

struct ObservedStream {
  stream: UnixStream,
  read_bytes: Arc<AtomicUsize>,
}

impl Read for ObservedStream {
  fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
    let n = self.stream.read(bytes)?;
    self.read_bytes.fetch_add(n, Ordering::SeqCst);
    Ok(n)
  }
}

impl Write for ObservedStream {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    self.stream.write(bytes)
  }
  fn flush(&mut self) -> std::io::Result<()> {
    self.stream.flush()
  }
}

struct Peer {
  client: UnixStream,
  events: tokio::sync::mpsc::Receiver<ActivityCmd>,
  sink: EventSink,
  token: CancellationToken,
  read_bytes: Arc<AtomicUsize>,
  done: mpsc::Receiver<()>,
  thread: Option<JoinHandle<()>>,
}

impl Peer {
  fn new(manager: &Arc<OAuthManager>, app: &str) -> Self {
    Self::with_capacity(manager, app, 64)
  }

  fn with_capacity(manager: &Arc<OAuthManager>, app: &str, capacity: usize) -> Self {
    let (server, client) = UnixStream::pair().expect("socketpair");
    server.set_nonblocking(true).expect("nonblocking pump");
    let read_bytes = Arc::new(AtomicUsize::new(0));
    let mut server = ObservedStream {
      stream: server,
      read_bytes: Arc::clone(&read_bytes),
    };
    client.set_read_timeout(Some(WAIT)).expect("read deadline");
    client
      .set_write_timeout(Some(WAIT))
      .expect("write deadline");
    let (sink, events) = EventSink::bounded(capacity);
    let token = CancellationToken::new();
    let mut facilitator = Facilitator {
      handshake: false,
      app: AppId::from(""),
      pid: 0,
      nonce: Value::Null,
      oauth: Arc::clone(manager),
      token: token.clone(),
      sink: sink.clone(),
      pids: Vec::new(),
    };
    let (finished, done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
      handle_stream(&mut facilitator, &mut server);
      let _ = finished.send(());
    });
    let mut peer = Self {
      client,
      events,
      sink,
      token,
      read_bytes,
      done,
      thread: Some(thread),
    };
    write_frame(
      &mut peer.client,
      PacketType::Handshake,
      &json!({"v":1,"client_id":app}).to_string(),
    );
    let ready = peer.reply();
    assert_eq!(ready["evt"], "READY");
    assert_eq!(ready["data"]["user"]["id"], RpcUser::default().id);
    peer
  }

  fn send(&mut self, command: Value) {
    write_frame(&mut self.client, PacketType::Frame, &command.to_string());
  }

  fn partial_frame(&mut self, bytes: &[u8]) {
    let expected = self.read_bytes.load(Ordering::SeqCst) + bytes.len();
    self.client.write_all(bytes).expect("partial frame");
    wait_until(|| self.read_bytes.load(Ordering::SeqCst) >= expected);
  }

  fn reply(&mut self) -> Value {
    let (packet, body) = read_frame(&mut self.client);
    assert_eq!(packet, PacketType::Frame as u32, "expected RPC reply");
    serde_json::from_str(&body).expect("JSON reply")
  }

  fn rpc(&mut self, command: Value) -> Value {
    self.send(command);
    self.reply()
  }

  fn publish(&mut self, pid: u64) {
    let reply = self.rpc(publish(pid, "presence"));
    assert_success(&reply, "presence");
    let event = recv_cmd(&mut self.events);
    assert_eq!(event.application_id.as_deref(), Some(APP));
    assert_eq!(event.args.as_ref().and_then(|args| args.pid), Some(pid));
    assert!(
      event
        .args
        .as_ref()
        .is_some_and(|args| matches!(args.activity, Some(Some(_))))
    );
  }

  fn join(&mut self) {
    self
      .done
      .recv_timeout(WAIT)
      .expect("IPC pump terminated promptly");
    self
      .thread
      .take()
      .expect("running thread")
      .join()
      .expect("IPC pump did not panic");
  }

  fn disconnect(&mut self) {
    self
      .client
      .shutdown(Shutdown::Both)
      .expect("disconnect peer");
    self.join();
  }
}

impl Drop for Peer {
  fn drop(&mut self) {
    self.token.cancel();
    let _ = self.client.shutdown(Shutdown::Both);
    if let Some(thread) = self.thread.take()
      && self.done.recv_timeout(WAIT).is_ok()
    {
      let _ = thread.join();
    }
  }
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
  let deadline = Instant::now() + WAIT;
  while !predicate() {
    assert!(
      Instant::now() < deadline,
      "condition did not become true before deadline"
    );
    std::thread::sleep(Duration::from_millis(2));
  }
}

fn authenticate(nonce: &str) -> Value {
  json!({"cmd":"AUTHENTICATE","args":{"access_token":"fake-token"},"nonce":nonce})
}

fn authorize(nonce: &str) -> Value {
  json!({"cmd":"AUTHORIZE","args":{"client_id":APP,"scopes":["rpc","identify"]},"nonce":nonce})
}

fn publish(pid: u64, nonce: &str) -> Value {
  json!({"cmd":"SET_ACTIVITY","args":{"pid":pid,"activity":{"name":"Game","type":0}},"nonce":nonce})
}

fn clear(pid: u64, nonce: &str) -> Value {
  json!({"cmd":"SET_ACTIVITY","args":{"pid":pid,"activity":null},"nonce":nonce})
}

fn get_user(nonce: &str) -> Value {
  json!({"cmd":"GET_USER","nonce":nonce})
}

fn session(app: &str, identify: bool, expires: SystemTime) -> RpcOAuthResult {
  let user = RpcUser {
    id: USER.to_owned(),
    username: "OAuth user".to_owned(),
    ..RpcUser::default()
  };
  let scopes = if identify {
    vec!["rpc".to_owned(), "identify".to_owned()]
  } else {
    vec!["rpc".to_owned()]
  };
  RpcOAuthResult {
    data: json!({"scopes":scopes,"user":if identify { Some(&user) } else { None }}),
    // Deliberately supply a user even without identify: the shared manager
    // must sanitize the session, and IPC must not substitute the fallback.
    session: Some(OAuthSession {
      application_id: AppId::from(app),
      user: Some(user),
      scopes,
      expires,
      access_token: "test-token".to_owned(),
    }),
  }
}

fn assert_success(reply: &Value, nonce: &str) {
  assert_ne!(reply["evt"], "ERROR", "unexpected RPC error: {reply}");
  assert_eq!(reply["nonce"], nonce);
}

fn assert_error(reply: &Value, code: u16, nonce: &str) {
  assert_eq!(reply["evt"], "ERROR", "expected RPC error: {reply}");
  assert_eq!(reply["data"]["code"], code);
  assert_eq!(reply["nonce"], nonce);
}

fn assert_clear(event: ActivityCmd, pid: u64) {
  assert_eq!(event.cmd, "SET_ACTIVITY");
  let args = event.args.expect("clear args");
  assert_eq!(args.pid, Some(pid));
  assert!(matches!(args.activity, Some(None)));
}

fn assert_closed(client: &mut UnixStream) {
  match client.read(&mut [0_u8; 1]) {
    Ok(0) => {}
    Err(error)
      if matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset
          | std::io::ErrorKind::ConnectionAborted
          | std::io::ErrorKind::BrokenPipe
      ) => {}
    other => panic!("expected a closed connection, got {other:?}"),
  }
}

#[test]
fn strict_pre_auth_gate_never_forwards_presence_or_secondary_commands() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::new(&fixture.manager, APP);
  for command in [
    publish(7, "denied"),
    clear(7, "denied"),
    get_user("denied"),
    json!({"cmd":"SUBSCRIBE","evt":"VOICE_STATE_UPDATE","nonce":"denied"}),
    json!({"cmd":"INVITE_BROWSER","args":{"code":"invite"},"nonce":"denied"}),
  ] {
    assert_error(&peer.rpc(command), 4006, "denied");
  }
  peer.disconnect();
  assert!(
    peer.events.try_recv().is_err(),
    "rejected pids must not gain cleanup rights"
  );
  assert!(fixture.requests.try_recv().is_err());
}

#[test]
fn authorize_does_not_authenticate_and_auth_success_enables_identity_and_presence() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::new(&fixture.manager, APP);
  peer.send(authorize("authorize"));
  fixture.request("AUTHORIZE", APP).finish(Ok(RpcOAuthResult {
    data: json!({"code":"fake-code"}),
    session: None,
  }));
  let reply = peer.reply();
  assert_success(&reply, "authorize");
  assert_eq!(reply["data"]["code"], "fake-code");
  assert_error(
    &peer.rpc(get_user("still-unauthenticated")),
    4006,
    "still-unauthenticated",
  );

  fixture.authenticate(&mut peer, true);
  let reply = peer.rpc(get_user("self"));
  assert_eq!(reply["data"]["id"], USER);
  assert_eq!(reply["data"]["username"], "OAuth user");
  let reply = peer.rpc(json!({"cmd":"GET_USER","args":{"user_id":USER},"nonce":"same-user"}));
  assert_eq!(reply["data"]["id"], USER);
  let reply =
    peer.rpc(json!({"cmd":"GET_USER","args":{"user_id":"someone-else"},"nonce":"other-user"}));
  assert_success(&reply, "other-user");
  assert!(reply["data"].is_null());
  peer.publish(7);
  peer.disconnect();
  assert_clear(recv_cmd(&mut peer.events), 7);
}

#[test]
fn authenticated_without_identify_returns_null_not_the_shared_fallback_identity() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::new(&fixture.manager, APP);
  fixture.authenticate(&mut peer, false);
  for args in [
    Value::Null,
    json!({"user_id":USER}),
    json!({"user_id":RpcUser::default().id}),
  ] {
    let reply = peer.rpc(json!({"cmd":"GET_USER","args":args,"nonce":"no-identify"}));
    assert_success(&reply, "no-identify");
    assert!(
      reply["data"].is_null(),
      "no identify scope must never reveal a user"
    );
  }
  // Authentication does not patch the daemon's cosmetic READY user.
  let _second = Peer::new(&fixture.manager, APP);
}

#[test]
fn sessions_are_connection_local_and_cross_application_overrides_are_rejected() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut a = Peer::new(&fixture.manager, APP);
  let mut b = Peer::new(&fixture.manager, APP);
  fixture.authenticate(&mut a, true);
  a.publish(41);
  assert_error(&b.rpc(get_user("isolation")), 4006, "isolation");
  assert_error(&b.rpc(clear(41, "not-owner")), 4006, "not-owner");
  fixture.authenticate(&mut b, true);
  assert_error(
    &b.rpc(clear(41, "authenticated-not-owner")),
    4006,
    "authenticated-not-owner",
  );
  assert!(b.events.try_recv().is_err());

  for mut command in [
    get_user("cross-app"),
    authorize("cross-app"),
    authenticate("cross-app"),
    publish(99, "cross-app"),
  ] {
    command["application_id"] = json!(OTHER_APP);
    assert_error(&a.rpc(command), 4007, "cross-app");
  }
  assert!(
    fixture.requests.try_recv().is_err(),
    "cross-app requests cannot reach the backend"
  );
  assert_eq!(a.rpc(get_user("unchanged"))["data"]["id"], USER);
  assert_success(&a.rpc(clear(41, "owned")), "owned");
  assert_clear(recv_cmd(&mut a.events), 41);
  a.disconnect();
  b.disconnect();
  assert!(a.events.try_recv().is_err());
  assert!(b.events.try_recv().is_err());
}

#[test]
fn failed_reauthentication_removes_identity_but_preserves_owned_cleanup() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::new(&fixture.manager, APP);
  fixture.authenticate(&mut peer, true);
  peer.publish(7);
  peer.send(authenticate("reauth"));
  let request = fixture.request("AUTHENTICATE", APP);
  // Admission, not completion, revokes the previous authenticated identity.
  assert_error(&peer.rpc(get_user("during-reauth")), 4006, "during-reauth");
  request.finish(Err(OAuthError::InvalidAccessToken));
  assert_error(&peer.reply(), 4009, "reauth");
  assert_error(&peer.rpc(clear(8, "foreign")), 4006, "foreign");
  assert_error(&peer.rpc(clear(0, "zero")), 4006, "zero");
  assert_success(&peer.rpc(clear(7, "cleanup")), "cleanup");
  assert_clear(recv_cmd(&mut peer.events), 7);
  assert_error(
    &peer.rpc(clear(7, "already-cleared")),
    4006,
    "already-cleared",
  );
  peer.disconnect();
  assert!(peer.events.try_recv().is_err());
}

#[test]
fn expiry_reports_an_error_without_fallback_and_allows_only_an_owned_clear() {
  for clear_first in [false, true] {
    let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
    let mut peer = Peer::new(&fixture.manager, APP);
    peer.send(authenticate("short-lived"));
    let request = fixture.request("AUTHENTICATE", APP);
    let expires = SystemTime::now() + Duration::from_secs(1);
    request.finish(Ok(session(APP, true, expires)));
    assert_success(&peer.reply(), "short-lived");
    peer.publish(7);
    wait_until(|| SystemTime::now() >= expires);
    if !clear_first {
      assert_error(&peer.rpc(get_user("expired")), 4009, "expired");
      assert_error(&peer.rpc(clear(8, "foreign")), 4006, "foreign");
    }
    assert_success(&peer.rpc(clear(7, "expired-cleanup")), "expired-cleanup");
    assert_clear(recv_cmd(&mut peer.events), 7);
    assert_error(
      &peer.rpc(publish(8, "no-new-presence")),
      4006,
      "no-new-presence",
    );
    peer.disconnect();
    assert!(peer.events.try_recv().is_err());
  }
}

#[test]
fn authenticate_rejects_backend_application_mismatch_and_backend_errors() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::new(&fixture.manager, APP);
  peer.send(authenticate("wrong-app"));
  fixture.request("AUTHENTICATE", APP).finish(Ok(session(
    OTHER_APP,
    true,
    SystemTime::now() + Duration::from_secs(60),
  )));
  assert_error(&peer.reply(), 4007, "wrong-app");
  assert_error(&peer.rpc(get_user("no-session")), 4006, "no-session");
  peer.send(authorize("bad-args"));
  fixture
    .request("AUTHORIZE", APP)
    .finish(Err(OAuthError::InvalidArguments));
  assert_error(&peer.reply(), 4000, "bad-args");
  fixture.authenticate(&mut peer, true);
  assert_eq!(peer.rpc(get_user("recovered"))["data"]["id"], USER);
}

#[test]
fn pending_authorize_keeps_ping_and_gate_responsive_and_refuses_a_second_oauth_request() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::new(&fixture.manager, APP);
  peer.send(authorize("first"));
  let pending = fixture.request("AUTHORIZE", APP);
  write_frame(&mut peer.client, PacketType::Ping, "still-responsive");
  assert_eq!(
    read_frame(&mut peer.client),
    (PacketType::Pong as u32, "still-responsive".to_owned())
  );
  assert_error(&peer.rpc(authenticate("second")), 5000, "second");
  assert_error(&peer.rpc(get_user("protected")), 4006, "protected");
  assert!(
    !pending.cancellation.is_cancelled(),
    "rejected admission must not cancel existing work"
  );
  assert!(fixture.requests.try_recv().is_err());
  pending.finish(Ok(RpcOAuthResult {
    data: json!({"code":"first-code"}),
    session: None,
  }));
  let reply = peer.reply();
  assert_success(&reply, "first");
  assert_eq!(reply["data"]["code"], "first-code");

  fixture.authenticate(&mut peer, true);
  peer.send(authorize("authenticated-pending"));
  let pending = fixture.request("AUTHORIZE", APP);
  assert_error(&peer.rpc(authenticate("busy-reauth")), 5000, "busy-reauth");
  assert_eq!(peer.rpc(get_user("session-preserved"))["data"]["id"], USER);
  pending.finish(Err(OAuthError::InvalidArguments));
  assert_error(&peer.reply(), 4000, "authenticated-pending");
  assert_eq!(
    peer.rpc(get_user("still-authenticated"))["data"]["id"],
    USER
  );
}

#[test]
fn oauth_reply_arrives_while_the_next_header_or_body_is_still_partial() {
  for split in [3, 11] {
    let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
    let mut peer = Peer::new(&fixture.manager, APP);
    peer.send(authenticate("async-auth"));
    let request = fixture.request("AUTHENTICATE", APP);
    let frame = encode(PacketType::Ping, "partial-ping-body");
    peer.partial_frame(&frame[..split]);
    request.finish(Ok(session(
      APP,
      true,
      SystemTime::now() + Duration::from_secs(60),
    )));
    // No new complete frame is sent to wake the reader.
    assert_success(&peer.reply(), "async-auth");
    peer
      .client
      .write_all(&frame[split..])
      .expect("remaining frame");
    assert_eq!(
      read_frame(&mut peer.client),
      (PacketType::Pong as u32, "partial-ping-body".to_owned())
    );
    assert_eq!(peer.rpc(get_user("authenticated"))["data"]["id"], USER);
  }
}

#[test]
fn every_second_handshake_closes_cancels_pending_work_and_clears_presence() {
  for second in [
    json!({"v":1,"client_id":APP}).to_string(),
    json!({"v":1,"client_id":OTHER_APP}).to_string(),
    "not JSON".to_owned(),
  ] {
    let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
    let mut peer = Peer::new(&fixture.manager, APP);
    fixture.authenticate(&mut peer, true);
    peer.publish(7);
    peer.send(authorize("pending"));
    let request = fixture.request("AUTHORIZE", APP);
    write_frame(&mut peer.client, PacketType::Handshake, &second);
    let (packet, body) = read_frame(&mut peer.client);
    assert_eq!(packet, PacketType::Close as u32);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["code"], 4000);
    peer.join();
    request.assert_cancelled();
    assert_clear(recv_cmd(&mut peer.events), 7);
    assert!(peer.events.try_recv().is_err());
    assert_closed(&mut peer.client);
  }
}

#[test]
fn close_eof_and_cancellation_cancel_pending_oauth_even_with_partial_frames() {
  for exit in 0..4 {
    let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
    let mut peer = Peer::new(&fixture.manager, APP);
    fixture.authenticate(&mut peer, true);
    peer.publish(7);
    peer.send(authorize("pending"));
    let request = fixture.request("AUTHORIZE", APP);
    match exit {
      0 => write_frame(&mut peer.client, PacketType::Close, "{}"),
      1 => {
        peer.client.shutdown(Shutdown::Write).expect("EOF");
      }
      2 | 3 => {
        let frame = encode(PacketType::Frame, &get_user("incomplete").to_string());
        let split = if exit == 2 { 3 } else { 11 };
        peer.partial_frame(&frame[..split]);
        if exit == 2 {
          peer
            .client
            .shutdown(Shutdown::Write)
            .expect("EOF mid-header");
        } else {
          peer.token.cancel();
        }
      }
      _ => unreachable!(),
    }
    peer.join();
    request.assert_cancelled();
    assert_clear(recv_cmd(&mut peer.events), 7);
    assert!(peer.events.try_recv().is_err());
  }
}

#[test]
fn late_worker_success_after_disconnect_cannot_resurrect_a_connection() {
  let fixture = OAuthFixture::with_backend(RpcAuthPolicy::Required, true);
  let mut peer = Peer::new(&fixture.manager, APP);
  peer.send(authenticate("late"));
  let request = fixture.request("AUTHENTICATE", APP);
  peer.disconnect();
  request.assert_cancelled();
  request.finish(Ok(session(
    APP,
    true,
    SystemTime::now() + Duration::from_secs(60),
  )));
  let mut fresh = Peer::new(&fixture.manager, APP);
  assert_error(
    &fresh.rpc(get_user("new-connection")),
    4006,
    "new-connection",
  );
  assert!(fresh.events.try_recv().is_err());
}

#[test]
fn failed_reply_tears_down_the_connection_and_its_published_presence() {
  for authenticate_again in [false, true] {
    let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
    let mut peer = Peer::new(&fixture.manager, APP);
    fixture.authenticate(&mut peer, true);
    peer.publish(7);
    let (command, result) = if authenticate_again {
      (
        authenticate("reply-will-fail"),
        session(APP, true, SystemTime::now() + Duration::from_secs(60)),
      )
    } else {
      (
        authorize("reply-will-fail"),
        RpcOAuthResult {
          data: json!({"code":"fake-code"}),
          session: None,
        },
      )
    };
    let verb = command["cmd"].as_str().unwrap().to_owned();
    peer.send(command);
    let request = fixture.request(&verb, APP);
    // Leave the write half open: EOF cannot be the reason the pump exits.
    peer
      .client
      .shutdown(Shutdown::Read)
      .expect("stop reading replies");
    request.finish(Ok(result));
    peer.join();
    assert_clear(recv_cmd(&mut peer.events), 7);
    assert!(peer.events.try_recv().is_err());
  }
}

#[test]
fn shed_publications_do_not_gain_owned_clear_or_disconnect_cleanup_rights() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::with_capacity(&fixture.manager, APP, 1);
  fixture.authenticate(&mut peer, true);
  peer.sink.emit(ActivityCmd::empty());
  assert_success(&peer.rpc(publish(8, "shed")), "shed");
  assert_eq!(peer.sink.dropped_total(), 1);
  assert_eq!(recv_cmd(&mut peer.events).cmd, "");
  peer.send(authenticate("failed-reauth"));
  fixture
    .request("AUTHENTICATE", APP)
    .finish(Err(OAuthError::InvalidAccessToken));
  assert_error(&peer.reply(), 4009, "failed-reauth");
  assert_error(&peer.rpc(clear(8, "not-owned")), 4006, "not-owned");
  peer.disconnect();
  assert!(peer.events.try_recv().is_err());
}

#[test]
fn shed_explicit_clears_keep_ownership_so_cleanup_can_retry_after_failed_auth() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut peer = Peer::with_capacity(&fixture.manager, APP, 1);
  fixture.authenticate(&mut peer, true);
  peer.publish(7);
  peer.sink.emit(ActivityCmd::empty());
  assert_success(&peer.rpc(clear(7, "shed-clear")), "shed-clear");
  assert_eq!(peer.sink.dropped_total(), 1);
  assert_eq!(recv_cmd(&mut peer.events).cmd, "");
  peer.send(authenticate("failed-reauth"));
  fixture
    .request("AUTHENTICATE", APP)
    .finish(Err(OAuthError::InvalidAccessToken));
  assert_error(&peer.reply(), 4009, "failed-reauth");
  assert_success(
    &peer.rpc(clear(7, "retry-owned-clear")),
    "retry-owned-clear",
  );
  assert_clear(recv_cmd(&mut peer.events), 7);
  peer.disconnect();
  assert!(peer.events.try_recv().is_err());
}

#[test]
fn daemon_wide_worker_capacity_rejection_does_not_erase_an_existing_session() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::Required);
  let mut authenticated = Peer::new(&fixture.manager, APP);
  fixture.authenticate(&mut authenticated, true);
  let mut peers = Vec::new();
  let mut requests = Vec::new();
  for _ in 0..4 {
    let mut peer = Peer::new(&fixture.manager, APP);
    peer.send(authorize("occupy-worker"));
    requests.push(fixture.request("AUTHORIZE", APP));
    peers.push(peer);
  }
  assert_error(
    &authenticated.rpc(authenticate("capacity")),
    5000,
    "capacity",
  );
  assert_eq!(
    authenticated.rpc(get_user("session-preserved"))["data"]["id"],
    USER
  );
  assert!(fixture.requests.try_recv().is_err());
  for (peer, request) in peers.iter_mut().zip(requests.iter()) {
    peer.disconnect();
    request.assert_cancelled();
  }
}

#[test]
fn default_permissive_policy_preserves_legacy_presence_identity_and_explicit_clears() {
  let fixture = OAuthFixture::new(RpcAuthPolicy::default());
  let mut peer = Peer::new(&fixture.manager, APP);
  assert_eq!(
    peer.rpc(get_user("legacy-user"))["data"]["id"],
    RpcUser::default().id
  );
  peer.publish(7);
  assert_success(&peer.rpc(clear(7, "legacy-clear")), "legacy-clear");
  assert_clear(recv_cmd(&mut peer.events), 7);
  assert_success(
    &peer.rpc(clear(8, "legacy-untracked-clear")),
    "legacy-untracked-clear",
  );
  assert_clear(recv_cmd(&mut peer.events), 8);
  peer.disconnect();
  assert!(
    peer.events.try_recv().is_err(),
    "legacy clears do not grant pid ownership"
  );
  assert!(
    fixture.requests.try_recv().is_err(),
    "no OAuth needed for legacy clients"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_and_drop_cancel_partial_oauth_without_stopping_shared_manager() {
  for drop_transport in [false, true] {
    for split in [3, 11] {
      let scratch = Scratch::new("oauth-teardown");
      let fixture = OAuthFixture::new(RpcAuthPolicy::Permissive);
      let (transport, mut events) = IpcTransport::bind_with_dirs_and_oauth(
        Arc::new(Mutex::new(RpcUser::default())),
        vec![scratch.sub("a")],
        Arc::clone(&fixture.manager),
      )
      .await
      .expect("bind with shared manager");
      let path = transport.socket_path().to_owned();
      let mut client = UnixStream::connect(&path).expect("connect");
      client.set_read_timeout(Some(WAIT)).expect("read deadline");
      write_frame(
        &mut client,
        PacketType::Handshake,
        &json!({"v":1,"client_id":APP}).to_string(),
      );
      assert!(read_frame(&mut client).1.contains("READY"));
      write_frame(
        &mut client,
        PacketType::Frame,
        &publish(7, "published").to_string(),
      );
      let reply: Value = serde_json::from_str(&read_frame(&mut client).1).unwrap();
      assert_success(&reply, "published");
      let _ = recv_cmd(&mut events);
      write_frame(
        &mut client,
        PacketType::Frame,
        &authorize("pending").to_string(),
      );
      let request = fixture.request("AUTHORIZE", APP);
      let frame = encode(PacketType::Frame, &get_user("partial").to_string());
      client.write_all(&frame[..split]).expect("partial frame");
      if drop_transport {
        drop(transport);
      } else {
        tokio::time::timeout(WAIT, transport.shutdown())
          .await
          .expect("bounded shutdown");
      }
      request.assert_cancelled();
      assert_clear(recv_cmd(&mut events), 7);
      assert_closed(&mut client);
      assert!(!std::path::Path::new(&path).exists());

      // IPC owns only its connections, never daemon-wide cancellation.
      let other = fixture.manager.connection(AppId::from(OTHER_APP));
      let task = other
        .start(serde_json::from_value(authenticate("other-transport")).unwrap())
        .expect("shared manager still admits work");
      fixture
        .request("AUTHENTICATE", OTHER_APP)
        .finish(Ok(session(
          OTHER_APP,
          true,
          SystemTime::now() + Duration::from_secs(60),
        )));
      assert!(task.recv().is_ok());
      assert!(other.session().unwrap().is_some());
      other.close();
    }
  }
}
