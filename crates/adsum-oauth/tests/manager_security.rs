//! Hermetic security and lifecycle checks through the public manager API.

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, Ordering},
  mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use adsum_oauth::{
  OAuthBackend, OAuthCancellation, OAuthConnection, OAuthError, OAuthManager, OAuthSession,
  OAuthTask, RpcAuthPolicy, RpcOAuthResult,
};
use adsum_types::{AppId, cmd::ActivityCmd, user::RpcUser};
use serde_json::{Value, json};

const APPLICATION_A: &str = "123456789012345678";
const APPLICATION_B: &str = "987654321098765432";
const USER_A: &str = "111111111111111111";
const USER_B: &str = "222222222222222222";
const WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(1);

fn poll_until<T>(description: &str, mut poll: impl FnMut() -> Option<T>) -> T {
  let deadline = Instant::now() + WAIT;
  loop {
    if let Some(value) = poll() {
      return value;
    }
    assert!(
      Instant::now() < deadline,
      "timed out waiting for {description}"
    );
    thread::sleep(POLL);
  }
}

fn join_bounded<T: Send + 'static>(worker: JoinHandle<T>) -> T {
  let (sender, receiver) = mpsc::sync_channel(1);
  // is_finished() alone cannot bound join() while thread-local destructors run.
  thread::spawn(move || {
    let _ = sender.send(worker.join());
  });
  poll_until("test thread completion", || match receiver.try_recv() {
    Ok(result) => Some(result),
    Err(TryRecvError::Empty) => None,
    Err(TryRecvError::Disconnected) => panic!("test join channel disconnected"),
  })
  .unwrap_or_else(|_| panic!("test thread panicked"))
}

fn task_result(task: &OAuthTask) -> Result<RpcOAuthResult, OAuthError> {
  poll_until("OAuth result", || task.try_result())
}

fn start_when_available(connection: &OAuthConnection, command: ActivityCmd) -> OAuthTask {
  poll_until("worker permit and connection idle", || {
    match connection.start(command.clone()) {
      Ok(task) => Some(task),
      Err(OAuthError::WorkerUnavailable | OAuthError::OperationInProgress) => None,
      Err(error) => panic!("unexpected admission error: {error:?}"),
    }
  })
}

fn command(verb: &str, args: Value) -> ActivityCmd {
  serde_json::from_value(json!({
    "cmd": verb,
    "args": args,
    "nonce": "manager-security"
  }))
  .expect("valid command fixture")
}

fn authenticate_command() -> ActivityCmd {
  command(
    "AUTHENTICATE",
    json!({"access_token": "hermetic-test-token"}),
  )
}

fn authorize_command(application: &str) -> ActivityCmd {
  command(
    "AUTHORIZE",
    json!({"client_id": application, "scopes": ["rpc"]}),
  )
}

fn publish_command() -> ActivityCmd {
  command(
    "SET_ACTIVITY",
    json!({"pid": 42, "activity": {"details": "hermetic"}}),
  )
}

fn clear_command() -> ActivityCmd {
  command("SET_ACTIVITY", json!({"pid": 42, "activity": null}))
}

fn session(application: &str, scopes: &[&str], user: Option<&str>) -> OAuthSession {
  OAuthSession {
    application_id: AppId::from(application),
    user: user.map(|id| RpcUser {
      id: id.to_owned(),
      username: "manager-security-user".to_owned(),
      discriminator: "0".to_owned(),
      global_name: None,
      avatar: None,
      avatar_decoration_data: None,
      bot: false,
      flags: 0,
      premium_type: 0,
    }),
    scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
    expires: SystemTime::now() + Duration::from_secs(60),
    access_token: "test-token".to_owned(),
  }
}

fn valid_session(application: &str) -> OAuthSession {
  session(application, &["rpc", "identify"], Some(USER_A))
}

fn authenticated_result(session: OAuthSession) -> RpcOAuthResult {
  RpcOAuthResult {
    data: json!({
      "application": {"id": session.application_id.as_ref()},
      "scopes": &session.scopes,
      "user": &session.user,
    }),
    session: Some(Arc::new(session)),
  }
}

fn authorized_result() -> RpcOAuthResult {
  RpcOAuthResult {
    data: json!({"code": "hermetic-authorization-code"}),
    session: None,
  }
}

enum Reply {
  Return(Box<Result<RpcOAuthResult, OAuthError>>),
  Panic,
}

struct Invocation {
  command: ActivityCmd,
  expected_client_id: String,
  cancellation: OAuthCancellation,
  reply: SyncSender<Reply>,
}

impl Invocation {
  fn complete(self, result: Result<RpcOAuthResult, OAuthError>) {
    assert!(
      self.reply.send(Reply::Return(Box::new(result))).is_ok(),
      "backend stopped before reply"
    );
  }

  fn panic(self) {
    assert!(
      self.reply.send(Reply::Panic).is_ok(),
      "backend stopped before panic request"
    );
  }
}

struct ControlledBackend {
  calls: Sender<Invocation>,
}

impl OAuthBackend for ControlledBackend {
  fn execute(
    &self,
    command: &ActivityCmd,
    expected_client_id: &str,
    cancellation: &OAuthCancellation,
  ) -> Result<RpcOAuthResult, OAuthError> {
    cancellation.check()?;
    let (reply, receiver) = mpsc::sync_channel(1);
    self
      .calls
      .send(Invocation {
        command: command.clone(),
        expected_client_id: expected_client_id.to_owned(),
        cancellation: cancellation.clone(),
        reply,
      })
      .map_err(|_| OAuthError::WorkerUnavailable)?;

    // Deliberately allow a successful late reply after cancellation, as a real
    // blocking backend can finish concurrently with disconnect or task drop.
    match receiver.recv_timeout(WAIT) {
      Ok(Reply::Return(result)) => *result,
      Ok(Reply::Panic) => panic!("hermetic backend panic"),
      Err(_) => Err(OAuthError::WorkerUnavailable),
    }
  }
}

struct Harness {
  manager: Arc<OAuthManager>,
  calls: Receiver<Invocation>,
}

impl Harness {
  fn new(policy: RpcAuthPolicy) -> Self {
    let (calls, receiver) = mpsc::channel();
    Self {
      manager: Arc::new(OAuthManager::new(
        policy,
        Arc::new(ControlledBackend { calls }),
      )),
      calls: receiver,
    }
  }

  fn with_capacity(policy: RpcAuthPolicy, capacity: usize) -> Self {
    let (calls, receiver) = mpsc::channel();
    Self {
      manager: Arc::new(OAuthManager::with_capacity(
        policy,
        Arc::new(ControlledBackend { calls }),
        NonZeroUsize::new(capacity).expect("nonzero test capacity"),
      )),
      calls: receiver,
    }
  }

  fn with_rate_limit(policy: RpcAuthPolicy, window: Duration) -> Self {
    let (calls, receiver) = mpsc::channel();
    Self {
      manager: Arc::new(OAuthManager::with_rate_limit(
        policy,
        Arc::new(ControlledBackend { calls }),
        NonZeroUsize::new(4).expect("nonzero test capacity"),
        window,
      )),
      calls: receiver,
    }
  }

  fn connection(&self, application: &str) -> OAuthConnection {
    self.manager.connection(AppId::from(application))
  }

  fn next_call(&self) -> Invocation {
    poll_until("backend invocation", || match self.calls.try_recv() {
      Ok(call) => Some(call),
      Err(TryRecvError::Empty) => None,
      Err(TryRecvError::Disconnected) => panic!("backend invocation channel disconnected"),
    })
  }

  fn assert_no_calls(&self) {
    assert!(matches!(self.calls.try_recv(), Err(TryRecvError::Empty)));
  }

  fn authenticate(&self, connection: &OAuthConnection, session: OAuthSession) -> RpcOAuthResult {
    let task = start_when_available(connection, authenticate_command());
    let call = self.next_call();
    assert_eq!(call.command.cmd, "AUTHENTICATE");
    assert_eq!(call.expected_client_id, connection.client_id().as_ref());
    call.complete(Ok(authenticated_result(session)));
    task_result(&task).expect("authentication fixture must succeed")
  }
}

impl Drop for Harness {
  fn drop(&mut self) {
    // Never perform an unbounded join during panic cleanup. Fake I/O is bounded
    // and outstanding invocations disconnect when their test scope unwinds.
    self.manager.cancel();
  }
}

fn assert_user(connection: &OAuthConnection, expected: &str) {
  let session = connection
    .session()
    .expect("live connection")
    .expect("authenticated session");
  assert_eq!(
    session.user.as_ref().map(|user| user.id.as_str()),
    Some(expected)
  );
}

fn assert_no_session(connection: &OAuthConnection) {
  assert!(matches!(connection.session(), Ok(None)));
}

fn assert_bad_reauthentication(result: RpcOAuthResult, expected: OAuthError) {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let connection = harness.connection(APPLICATION_A);
  harness.authenticate(&connection, valid_session(APPLICATION_A));
  let task = start_when_available(&connection, authenticate_command());
  let call = harness.next_call();
  assert_no_session(&connection);
  call.complete(Ok(result));
  assert_eq!(task_result(&task).unwrap_err(), expected);
  assert_no_session(&connection);
  assert_eq!(
    connection.check_command(&publish_command(), false),
    Err(OAuthError::NotAuthenticated)
  );
}

fn concurrent_starts(connections: Vec<OAuthConnection>) -> Vec<Result<OAuthTask, OAuthError>> {
  let start = Arc::new(AtomicBool::new(false));
  let workers: Vec<_> = connections
    .into_iter()
    .map(|connection| {
      let start = Arc::clone(&start);
      thread::spawn(move || {
        poll_until("concurrent start signal", || {
          start.load(Ordering::Acquire).then_some(())
        });
        connection.start(authenticate_command())
      })
    })
    .collect();
  start.store(true, Ordering::Release);
  workers.into_iter().map(join_bounded).collect()
}

#[test]
fn required_policy_only_exempts_oauth_and_an_owned_explicit_activity_clear() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  let connection = harness.connection(APPLICATION_A);
  assert_eq!(harness.manager.policy(), RpcAuthPolicy::Required);
  for verb in ["GET_USER", "SUBSCRIBE", "GET_GUILD"] {
    assert_eq!(
      connection.check_command(&command(verb, json!({})), false),
      Err(OAuthError::NotAuthenticated)
    );
  }
  assert_eq!(
    connection.check_command(&publish_command(), false),
    Err(OAuthError::NotAuthenticated)
  );
  assert_eq!(
    connection.check_command(&authenticate_command(), false),
    Ok(())
  );
  assert_eq!(
    connection.check_command(&authorize_command(APPLICATION_A), false),
    Ok(())
  );
  assert_eq!(connection.check_command(&clear_command(), true), Ok(()));
  assert_eq!(
    connection.check_command(&clear_command(), false),
    Err(OAuthError::NotAuthenticated)
  );
  assert_eq!(
    connection.check_command(&publish_command(), true),
    Err(OAuthError::NotAuthenticated)
  );
  assert_eq!(
    connection.check_command(&command("SET_ACTIVITY", json!({"pid": 42})), true),
    Err(OAuthError::NotAuthenticated)
  );
  assert_eq!(
    connection.check_command(&command("GET_USER", json!({"activity": null})), true),
    Err(OAuthError::NotAuthenticated)
  );
  let mut foreign_clear = clear_command();
  foreign_clear.application_id = Some(AppId::from(APPLICATION_B));
  assert_eq!(
    connection.check_command(&foreign_clear, true),
    Err(OAuthError::ApplicationMismatch)
  );
  harness.assert_no_calls();
}

#[test]
fn permissive_policy_preserves_legacy_commands_but_never_trusts_oauth_application_overrides() {
  let harness = Harness::new(RpcAuthPolicy::Permissive);
  let connection = harness.connection(APPLICATION_A);
  assert_eq!(harness.manager.policy(), RpcAuthPolicy::Permissive);
  for verb in ["SET_ACTIVITY", "GET_USER", "SUBSCRIBE"] {
    let mut command = command(verb, json!({}));
    command.application_id = Some(AppId::from(APPLICATION_B));
    assert_eq!(connection.check_command(&command, false), Ok(()));
  }
  for mut command in [authenticate_command(), authorize_command(APPLICATION_A)] {
    command.application_id = Some(AppId::from(APPLICATION_B));
    assert_eq!(
      connection.check_command(&command, false),
      Err(OAuthError::ApplicationMismatch)
    );
    assert_eq!(
      connection.start(command).unwrap_err(),
      OAuthError::ApplicationMismatch
    );
  }
  assert_no_session(&connection);
  harness.assert_no_calls();
}

#[test]
fn authenticated_application_binding_applies_to_both_policies_and_cleanup() {
  for policy in [RpcAuthPolicy::Required, RpcAuthPolicy::Permissive] {
    let harness = Harness::new(policy);
    let connection = harness.connection(APPLICATION_A);
    harness.authenticate(&connection, valid_session(APPLICATION_A));
    assert_eq!(connection.client_id().as_ref(), APPLICATION_A);
    assert_eq!(connection.check_command(&publish_command(), false), Ok(()));
    for verb in ["GET_USER", "SET_ACTIVITY", "AUTHENTICATE", "AUTHORIZE"] {
      let mut command = command(verb, json!({}));
      command.application_id = Some(AppId::from(APPLICATION_B));
      assert_eq!(
        connection.check_command(&command, false),
        Err(OAuthError::ApplicationMismatch)
      );
    }
    let mut clear = clear_command();
    clear.application_id = Some(AppId::from(APPLICATION_B));
    assert_eq!(
      connection.check_command(&clear, true),
      Err(OAuthError::ApplicationMismatch)
    );
    let mut retry = authenticate_command();
    retry.application_id = Some(AppId::from(APPLICATION_B));
    assert_eq!(
      connection.start(retry).unwrap_err(),
      OAuthError::ApplicationMismatch
    );
    assert_user(&connection, USER_A);
    harness.assert_no_calls();
  }
}

#[test]
fn identify_scope_without_a_user_never_fabricates_an_identity() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  let connection = harness.connection(APPLICATION_A);
  let result = harness.authenticate(
    &connection,
    session(APPLICATION_A, &["rpc", "identify"], None),
  );
  assert!(result.session.as_ref().unwrap().user.is_none());
  assert!(result.data["user"].is_null());
  let stored = connection.session().unwrap().unwrap();
  assert_eq!(stored.application_id.as_ref(), APPLICATION_A);
  assert!(stored.user.is_none());
  assert_eq!(connection.check_command(&publish_command(), false), Ok(()));
}

#[test]
fn rpc_without_identify_strips_even_a_backend_supplied_session_user() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  let connection = harness.connection(APPLICATION_A);
  let result = harness.authenticate(&connection, session(APPLICATION_A, &["rpc"], Some(USER_A)));
  assert!(result.session.as_ref().unwrap().user.is_none());
  assert!(connection.session().unwrap().unwrap().user.is_none());
  assert_eq!(connection.check_command(&publish_command(), false), Ok(()));
}

#[test]
fn sessions_are_connection_local_even_for_the_same_application_and_snapshots_are_detached() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  let first = harness.connection(APPLICATION_A);
  let alias = first.clone();
  let same_application = harness.connection(APPLICATION_A);
  let other_application = harness.connection(APPLICATION_B);
  harness.authenticate(&first, valid_session(APPLICATION_A));
  assert_user(&alias, USER_A);
  for connection in [&same_application, &other_application] {
    assert_no_session(connection);
    assert_eq!(
      connection.check_command(&publish_command(), false),
      Err(OAuthError::NotAuthenticated)
    );
  }

  let mut snapshot = (*first.session().unwrap().unwrap()).clone();
  snapshot.application_id = AppId::from(APPLICATION_B);
  snapshot.user.as_mut().unwrap().id = USER_B.to_owned();
  snapshot.scopes.clear();
  snapshot.expires = UNIX_EPOCH;
  assert_user(&first, USER_A);
  assert_eq!(
    first.session().unwrap().unwrap().application_id.as_ref(),
    APPLICATION_A
  );

  harness.authenticate(
    &same_application,
    session(APPLICATION_A, &["rpc", "identify"], Some(USER_B)),
  );
  assert_user(&first, USER_A);
  assert_user(&same_application, USER_B);
  alias.close();
  assert_eq!(first.session().unwrap_err(), OAuthError::Cancelled);
  assert_eq!(alias.session().unwrap_err(), OAuthError::Cancelled);
  assert_user(&same_application, USER_B);
  assert_no_session(&harness.connection(APPLICATION_A));
}

#[test]
fn authorize_returns_a_code_without_authenticating_the_connection() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  let connection = harness.connection(APPLICATION_A);
  let task = connection.start(authorize_command(APPLICATION_A)).unwrap();
  let call = harness.next_call();
  assert_eq!(call.command.cmd, "AUTHORIZE");
  assert_eq!(call.expected_client_id, APPLICATION_A);
  call.complete(Ok(authorized_result()));
  let result = task_result(&task).unwrap();
  assert_eq!(result.data["code"], "hermetic-authorization-code");
  assert!(result.session.is_none());
  assert_no_session(&connection);
  assert_eq!(
    connection.check_command(&publish_command(), false),
    Err(OAuthError::NotAuthenticated)
  );
}

#[test]
fn a_backend_session_for_a_different_application_cannot_replace_an_existing_identity() {
  assert_bad_reauthentication(
    authenticated_result(valid_session(APPLICATION_B)),
    OAuthError::ApplicationMismatch,
  );
}

#[test]
fn a_backend_session_without_rpc_scope_cannot_replace_an_existing_identity() {
  assert_bad_reauthentication(
    authenticated_result(session(APPLICATION_A, &["identify"], Some(USER_B))),
    OAuthError::MissingRpcScope,
  );
}

#[test]
fn an_expired_backend_session_cannot_replace_an_existing_identity() {
  let mut expired = valid_session(APPLICATION_A);
  expired.expires = UNIX_EPOCH;
  assert!(expired.is_expired());
  assert_bad_reauthentication(
    authenticated_result(expired),
    OAuthError::InvalidAccessToken,
  );
}

#[test]
fn authenticate_requires_session_metadata_even_if_the_backend_reports_success() {
  assert_bad_reauthentication(authorized_result(), OAuthError::UnexpectedDiscordResponse);
}

#[test]
fn admitted_failed_reauthentication_forgets_the_old_identity_under_both_policies() {
  for policy in [RpcAuthPolicy::Required, RpcAuthPolicy::Permissive] {
    let harness = Harness::new(policy);
    let connection = harness.connection(APPLICATION_A);
    harness.authenticate(&connection, valid_session(APPLICATION_A));
    let task = start_when_available(&connection, authenticate_command());
    let call = harness.next_call();
    assert_no_session(&connection);
    call.complete(Err(OAuthError::InvalidAccessToken));
    assert_eq!(
      task_result(&task).unwrap_err(),
      OAuthError::InvalidAccessToken
    );
    assert_no_session(&connection);
    let expected = if policy == RpcAuthPolicy::Required {
      Err(OAuthError::NotAuthenticated)
    } else {
      Ok(())
    };
    assert_eq!(
      connection.check_command(&publish_command(), false),
      expected
    );
    assert_eq!(connection.check_command(&clear_command(), true), Ok(()));
  }
}

#[test]
fn expiry_is_reported_once_without_legacy_fallback_on_the_discovering_request() {
  for policy in [RpcAuthPolicy::Required, RpcAuthPolicy::Permissive] {
    let harness = Harness::new(policy);
    let connection = harness.connection(APPLICATION_A);
    let mut expiring = valid_session(APPLICATION_A);
    expiring.expires = SystemTime::now() + Duration::from_millis(500);
    let expires = expiring.expires;
    harness.authenticate(&connection, expiring);
    poll_until("session expiry", || {
      (SystemTime::now() >= expires).then_some(())
    });
    assert_eq!(
      connection.check_command(&publish_command(), false),
      Err(OAuthError::InvalidAccessToken)
    );
    assert_no_session(&connection);
    let expected = if policy == RpcAuthPolicy::Required {
      Err(OAuthError::NotAuthenticated)
    } else {
      Ok(())
    };
    assert_eq!(
      connection.check_command(&publish_command(), false),
      expected
    );
    assert_eq!(connection.check_command(&clear_command(), true), Ok(()));
  }
}

#[test]
fn owned_cleanup_and_reauthentication_can_themselves_discover_expiry() {
  for cleanup in [true, false] {
    let harness = Harness::new(RpcAuthPolicy::Required);
    let connection = harness.connection(APPLICATION_A);
    let mut expiring = valid_session(APPLICATION_A);
    expiring.expires = SystemTime::now() + Duration::from_millis(500);
    let expires = expiring.expires;
    harness.authenticate(&connection, expiring);
    poll_until("session expiry", || {
      (SystemTime::now() >= expires).then_some(())
    });
    if cleanup {
      assert_eq!(connection.check_command(&clear_command(), true), Ok(()));
      assert_no_session(&connection);
    } else {
      let task = connection.start(authenticate_command()).unwrap();
      let call = harness.next_call();
      assert_no_session(&connection);
      call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
      task_result(&task).unwrap();
      assert_user(&connection, USER_A);
    }
  }
}

#[test]
fn early_rejected_concurrent_authenticate_does_not_erase_a_live_session() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let connection = harness.connection(APPLICATION_A);
  harness.authenticate(&connection, valid_session(APPLICATION_A));
  let task = start_when_available(&connection, authorize_command(APPLICATION_A));
  let call = harness.next_call();
  assert_user(&connection, USER_A);
  assert_eq!(
    connection
      .clone()
      .start(authenticate_command())
      .unwrap_err(),
    OAuthError::OperationInProgress
  );
  assert_user(&connection, USER_A);
  assert_eq!(connection.check_command(&publish_command(), false), Ok(()));
  harness.assert_no_calls();
  call.complete(Err(OAuthError::AuthorizationDenied));
  assert_eq!(
    task_result(&task).unwrap_err(),
    OAuthError::AuthorizationDenied
  );
  assert_user(&connection, USER_A);
}

#[test]
fn global_capacity_rejection_does_not_erase_a_live_session() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let authenticated = harness.connection(APPLICATION_A);
  let blocker = harness.connection(APPLICATION_B);
  harness.authenticate(&authenticated, valid_session(APPLICATION_A));
  let pending = start_when_available(&blocker, authorize_command(APPLICATION_B));
  let call = harness.next_call();
  assert_eq!(call.expected_client_id, APPLICATION_B);
  assert_eq!(
    authenticated.start(authenticate_command()).unwrap_err(),
    OAuthError::WorkerUnavailable
  );
  assert_user(&authenticated, USER_A);
  assert_eq!(
    authenticated.check_command(&publish_command(), false),
    Ok(())
  );
  harness.assert_no_calls();
  call.complete(Ok(authorized_result()));
  task_result(&pending).unwrap();
  assert_user(&authenticated, USER_A);
}

#[test]
fn simultaneous_starts_on_connection_clones_admit_exactly_one_operation() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  let connection = harness.connection(APPLICATION_A);
  let results = concurrent_starts((0..8).map(|_| connection.clone()).collect());
  let mut accepted = Vec::new();
  let mut rejected = 0;
  for result in results {
    match result {
      Ok(task) => accepted.push(task),
      Err(error) => {
        assert_eq!(error, OAuthError::OperationInProgress);
        rejected += 1;
      }
    }
  }
  assert_eq!(accepted.len(), 1);
  assert_eq!(rejected, 7);
  let call = harness.next_call();
  harness.assert_no_calls();
  call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  task_result(&accepted[0]).unwrap();
  assert_user(&connection, USER_A);
}

#[test]
fn simultaneous_independent_connections_share_one_global_capacity_budget() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 2);
  let connections = (0..8).map(|_| harness.connection(APPLICATION_A)).collect();
  let mut accepted = Vec::new();
  let mut rejected = 0;
  for result in concurrent_starts(connections) {
    match result {
      Ok(task) => accepted.push(task),
      Err(error) => {
        assert_eq!(error, OAuthError::WorkerUnavailable);
        rejected += 1;
      }
    }
  }
  assert_eq!(accepted.len(), 2);
  assert_eq!(rejected, 6);
  let first = harness.next_call();
  let second = harness.next_call();
  harness.assert_no_calls();
  first.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  second.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  for task in accepted {
    task_result(&task).unwrap();
  }
}

#[test]
fn task_cancel_discards_a_late_success_and_the_connection_can_retry() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let connection = harness.connection(APPLICATION_A);
  let task = connection.start(authenticate_command()).unwrap();
  let call = harness.next_call();
  task.cancel();
  assert_eq!(call.cancellation.check(), Err(OAuthError::Cancelled));
  call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  assert_eq!(task_result(&task).unwrap_err(), OAuthError::Cancelled);
  assert_no_session(&connection);
  harness.authenticate(&connection, valid_session(APPLICATION_A));
  assert_user(&connection, USER_A);
}

#[test]
fn dropping_a_pending_task_cancels_it_without_allowing_a_late_result_to_authenticate() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let connection = harness.connection(APPLICATION_A);
  let task = connection.start(authenticate_command()).unwrap();
  let call = harness.next_call();
  drop(task);
  assert_eq!(call.cancellation.check(), Err(OAuthError::Cancelled));
  assert_eq!(
    connection.start(authenticate_command()).unwrap_err(),
    OAuthError::OperationInProgress
  );
  call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));

  // AUTHORIZE does not clear a session, so this probe cannot hide an accidental
  // resurrection while waiting for the cancelled worker to release its permit.
  let probe = start_when_available(&connection, authorize_command(APPLICATION_A));
  let call = harness.next_call();
  assert_no_session(&connection);
  assert_eq!(
    connection.check_command(&publish_command(), false),
    Err(OAuthError::NotAuthenticated)
  );
  call.complete(Ok(authorized_result()));
  task_result(&probe).unwrap();
  assert_no_session(&connection);
}

#[test]
fn close_cancels_pending_work_and_late_results_cannot_resurrect_or_cross_connections() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let connection = harness.connection(APPLICATION_A);
  let alias = connection.clone();
  let other = harness.connection(APPLICATION_B);
  let task = connection.start(authenticate_command()).unwrap();
  let call = harness.next_call();
  alias.close();
  connection.close();
  assert_eq!(call.cancellation.check(), Err(OAuthError::Cancelled));
  for connection in [&connection, &alias] {
    assert_eq!(connection.session().unwrap_err(), OAuthError::Cancelled);
    assert_eq!(
      connection.check_command(&clear_command(), true),
      Err(OAuthError::Cancelled)
    );
    assert_eq!(
      connection.start(authenticate_command()).unwrap_err(),
      OAuthError::Cancelled
    );
  }
  // Cancellation must not return the global permit while backend work is live.
  assert_eq!(
    other.start(authenticate_command()).unwrap_err(),
    OAuthError::WorkerUnavailable
  );
  harness.assert_no_calls();
  call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  assert_eq!(task_result(&task).unwrap_err(), OAuthError::Cancelled);
  harness.authenticate(&other, valid_session(APPLICATION_B));
  assert_user(&other, USER_A);
  assert_eq!(connection.session().unwrap_err(), OAuthError::Cancelled);
  let replacement = harness.connection(APPLICATION_A);
  assert_no_session(&replacement);
  assert_eq!(
    replacement.check_command(&publish_command(), false),
    Err(OAuthError::NotAuthenticated)
  );
}

#[test]
fn cancel_rejects_future_work_and_even_cleanup_without_waiting_for_a_pending_backend() {
  let harness = Harness::new(RpcAuthPolicy::Permissive);
  let connection = harness.connection(APPLICATION_A);
  let task = connection.start(authenticate_command()).unwrap();
  let call = harness.next_call();
  harness.manager.cancel();
  assert_eq!(call.cancellation.check(), Err(OAuthError::Cancelled));
  assert_eq!(connection.session().unwrap_err(), OAuthError::Cancelled);
  assert_eq!(
    connection.check_command(&publish_command(), false),
    Err(OAuthError::Cancelled)
  );
  assert_eq!(
    connection.check_command(&clear_command(), true),
    Err(OAuthError::Cancelled)
  );
  let future = harness.connection(APPLICATION_B);
  assert_eq!(
    future.start(authorize_command(APPLICATION_B)).unwrap_err(),
    OAuthError::Cancelled
  );
  assert_eq!(
    future.start(authenticate_command()).unwrap_err(),
    OAuthError::Cancelled
  );
  harness.assert_no_calls();
  call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  assert_eq!(task_result(&task).unwrap_err(), OAuthError::Cancelled);
}

#[test]
fn shutdown_joins_pending_work_and_permanently_rejects_existing_and_future_connections() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let established = harness.connection(APPLICATION_A);
  harness.authenticate(&established, valid_session(APPLICATION_A));
  let pending = harness.connection(APPLICATION_B);
  let task = start_when_available(&pending, authenticate_command());
  let call = harness.next_call();
  let manager = Arc::clone(&harness.manager);
  let shutdown = thread::spawn(move || manager.shutdown());
  poll_until("shutdown cancellation", || {
    call.cancellation.is_cancelled().then_some(())
  });
  assert!(
    !shutdown.is_finished(),
    "shutdown must still be joining the controlled backend"
  );
  assert_eq!(established.session().unwrap_err(), OAuthError::Cancelled);
  assert_eq!(
    established.check_command(&publish_command(), false),
    Err(OAuthError::Cancelled)
  );
  assert_eq!(
    pending.start(authorize_command(APPLICATION_B)).unwrap_err(),
    OAuthError::Cancelled
  );
  let future = harness.connection(APPLICATION_A);
  assert_eq!(
    future.start(authenticate_command()).unwrap_err(),
    OAuthError::Cancelled
  );
  call.complete(Ok(authenticated_result(valid_session(APPLICATION_B))));
  assert_eq!(task_result(&task).unwrap_err(), OAuthError::Cancelled);
  join_bounded(shutdown);
  assert_eq!(pending.session().unwrap_err(), OAuthError::Cancelled);
  assert_eq!(
    future.start(authorize_command(APPLICATION_A)).unwrap_err(),
    OAuthError::Cancelled
  );
  let manager = Arc::clone(&harness.manager);
  join_bounded(thread::spawn(move || manager.shutdown()));
}

#[test]
fn backend_panics_fail_closed_and_release_both_connection_and_global_permits() {
  let harness = Harness::with_capacity(RpcAuthPolicy::Required, 1);
  let connection = harness.connection(APPLICATION_A);
  let task = connection.start(authenticate_command()).unwrap();
  harness.next_call().panic();
  assert_eq!(
    task_result(&task).unwrap_err(),
    OAuthError::WorkerUnavailable
  );
  assert_no_session(&connection);
  assert_eq!(
    connection.check_command(&publish_command(), false),
    Err(OAuthError::NotAuthenticated)
  );
  harness.authenticate(&connection, valid_session(APPLICATION_A));
  assert_user(&connection, USER_A);
}

#[test]
fn invalid_connection_ids_are_rejected_without_invoking_the_backend() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  for (application, expected) in [
    ("", OAuthError::MissingClientId),
    ("not-a-snowflake", OAuthError::InvalidClientId),
    ("123456789012345678901", OAuthError::InvalidClientId),
    ("123\n456", OAuthError::InvalidClientId),
  ] {
    let connection = harness.connection(application);
    assert_eq!(
      connection.start(authenticate_command()).unwrap_err(),
      expected
    );
    assert_no_session(&connection);
  }
  harness.assert_no_calls();
}

#[test]
fn non_oauth_work_is_rejected_before_it_can_invalidate_an_existing_session() {
  let harness = Harness::new(RpcAuthPolicy::Required);
  let connection = harness.connection(APPLICATION_A);
  harness.authenticate(&connection, valid_session(APPLICATION_A));
  assert_eq!(
    connection.start(publish_command()).unwrap_err(),
    OAuthError::InvalidArguments
  );
  assert_user(&connection, USER_A);
  harness.assert_no_calls();
}

#[test]
fn worker_reaping_does_not_hold_the_global_lock_during_reentrant_thread_local_teardown() {
  struct ExitProbe {
    entered: Sender<()>,
    release: Receiver<()>,
    reentrant: OAuthConnection,
    task: Sender<Result<OAuthTask, OAuthError>>,
    outcome: Sender<bool>,
  }

  impl Drop for ExitProbe {
    fn drop(&mut self) {
      let _ = self.entered.send(());
      if self.release.recv_timeout(WAIT).is_err() {
        return;
      }
      let reentrant = self.reentrant.clone();
      let task = self.task.clone();
      let returned = Arc::new(AtomicBool::new(false));
      let completed = Arc::clone(&returned);
      let worker = thread::Builder::new().spawn(move || {
        let result = reentrant.start(authenticate_command());
        let _ = task.send(result);
        completed.store(true, Ordering::Release);
      });
      if worker.is_err() {
        let _ = self.outcome.send(false);
        return;
      }

      // A real destructor awaiting this reentrant start would deadlock if the
      // reaper holds the manager lock. Escape after one second, then clean up
      // both operations before failing the assertion in the test thread.
      let deadline = Instant::now() + Duration::from_secs(1);
      while !returned.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(POLL);
      }
      let _ = self.outcome.send(returned.load(Ordering::Acquire));
    }
  }

  thread_local! {
    static ON_EXIT: RefCell<Option<ExitProbe>> = const { RefCell::new(None) };
  }

  struct ExitBackend {
    inner: ControlledBackend,
    on_exit: Mutex<Option<ExitProbe>>,
  }

  impl OAuthBackend for ExitBackend {
    fn execute(
      &self,
      command: &ActivityCmd,
      expected_client_id: &str,
      cancellation: &OAuthCancellation,
    ) -> Result<RpcOAuthResult, OAuthError> {
      if let Some(probe) = self.on_exit.lock().unwrap().take() {
        ON_EXIT.with(|slot| *slot.borrow_mut() = Some(probe));
      }
      self
        .inner
        .execute(command, expected_client_id, cancellation)
    }
  }

  let (calls, receiver) = mpsc::channel();
  let backend = Arc::new(ExitBackend {
    inner: ControlledBackend { calls },
    on_exit: Mutex::new(None),
  });
  let harness = Harness {
    manager: Arc::new(OAuthManager::with_capacity(
      RpcAuthPolicy::Required,
      backend.clone(),
      NonZeroUsize::new(3).unwrap(),
    )),
    calls: receiver,
  };
  let (entered, entering) = mpsc::channel();
  let (release, released) = mpsc::channel();
  let (task, reentrant_task) = mpsc::channel();
  let (outcome, teardown_outcome) = mpsc::channel();
  *backend.on_exit.lock().unwrap() = Some(ExitProbe {
    entered,
    release: released,
    reentrant: harness.connection(APPLICATION_A),
    task,
    outcome,
  });

  let first = harness.connection(APPLICATION_A);
  harness.authenticate(&first, valid_session(APPLICATION_A));
  poll_until("worker thread-local teardown", || entering.try_recv().ok());

  let second = harness.connection(APPLICATION_B);
  let starting = thread::spawn(move || second.start(authenticate_command()));
  let second_call = harness.next_call();
  release.send(()).unwrap();
  let progressed = poll_until("reentrant teardown outcome", || {
    teardown_outcome.try_recv().ok()
  });
  let second_task = join_bounded(starting).unwrap();
  let third_task = poll_until("reentrant task admission", || {
    reentrant_task.try_recv().ok()
  })
  .unwrap();
  let third_call = harness.next_call();
  second_call.complete(Ok(authenticated_result(valid_session(APPLICATION_B))));
  third_call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  task_result(&second_task).unwrap();
  task_result(&third_task).unwrap();
  assert!(
    progressed,
    "worker reaping held the global workers mutex while TLS teardown awaited another start"
  );
}

#[test]
fn rpc_oauth_result_debug_redacts_credentials_and_session_contents() {
  let mut session = valid_session(APPLICATION_A);
  session.user.as_mut().unwrap().username = "secret-session-username".to_owned();
  session.scopes.push("secret-session-scope".to_owned());
  let result = RpcOAuthResult {
    data: json!({
      "code": "secret-authorization-code",
      "access_token": "secret-access-token",
      "nested": {"refresh_token": "secret-refresh-token"},
      "user": {"id": USER_A}
    }),
    session: Some(Arc::new(session)),
  };
  for debug in [
    format!("{result:?}"),
    format!("{result:#?}"),
    format!("{:?}", Ok::<_, OAuthError>(result.clone())),
  ] {
    assert!(debug.contains("REDACTED"));
    for secret in [
      "secret-authorization-code",
      "secret-access-token",
      "secret-refresh-token",
      "secret-session-username",
      "secret-session-scope",
      APPLICATION_A,
      USER_A,
    ] {
      assert!(
        !debug.contains(secret),
        "Debug exposed a secret or session field"
      );
    }
  }
}

/// The per-connection rate limiter rejects the second command inside its
/// window with `RateLimited`, counts the rejection, and leaves the session
/// untouched. Uses the injected backend, so no network call is made.
#[test]
fn connection_rate_limit_rejects_the_second_oauth_command_and_counts_it() {
  let harness = Harness::with_rate_limit(RpcAuthPolicy::Permissive, Duration::from_secs(60));
  let connection = harness.connection(APPLICATION_A);
  let task = connection
    .start(command(
      "AUTHENTICATE",
      json!({"access_token": "rate-limit-token-1"}),
    ))
    .expect("first command admitted");
  let call = harness.next_call();
  call.complete(Ok(authenticated_result(valid_session(APPLICATION_A))));
  task_result(&task).expect("first command succeeds");
  let second = connection.start(command(
    "AUTHENTICATE",
    json!({"access_token": "rate-limit-token-2"}),
  ));
  assert_eq!(second.unwrap_err(), OAuthError::RateLimited);
  assert_eq!(harness.manager.metrics().rate_limited, 1);
  assert_eq!(harness.manager.metrics().authenticate_total, 1);
  harness.assert_no_calls();
}
