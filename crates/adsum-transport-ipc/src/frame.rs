//! Discord IPC wire protocol: framing, handshake, command dispatch.
//!
//! Every connection owns an OAuth handle and a single reply mailbox. The
//! nonblocking pump keeps servicing frames, EOF and shutdown during browser
//! authorization; both platform servers use exactly the same framing logic.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use adsum_oauth::{OAuthConnection, OAuthError, OAuthManager, OAuthTask, RpcAuthPolicy};
use adsum_protocol::commands;
use adsum_types::AppId;
use adsum_types::cmd::{ActivityCmd, ActivityCmdArgs};
use adsum_types::user::RpcUser;
use serde_json::Value;

use crate::EventSink;
use crate::codec::{DecodeError, Decoder, ReadFrame, Replies};

/// Per-connection protocol state. Implemented by the server; object-safe so
/// `handle_stream` stays generic over platforms.
pub trait IpcFacilitator: Send {
  /// Whether the handshake completed on this connection.
  fn handshake(&self) -> bool;
  /// Mark the handshake complete (cleared on close).
  fn set_handshake(&mut self, handshake: bool);

  /// Application id from the handshake (refcounted: the forward path
  /// stamps it onto every command the connection sends).
  fn client_id(&self) -> AppId;
  /// Remember the handshake application id.
  fn set_client_id(&mut self, client_id: AppId);

  /// Last admitted publication's pid (never an unowned/refused pid).
  fn pid(&self) -> u64;
  /// Remember the last admitted publication's pid.
  fn set_pid(&mut self, pid: u64);

  /// Last activity nonce, as the client sent it.
  fn nonce(&self) -> Value;
  /// Remember the last activity nonce.
  fn set_nonce(&mut self, nonce: Value);

  /// The `DISPATCH`/`READY` frame for new connections (reflects the
  /// current shared user, including `SET_USER` patches).
  fn user_payload(&self) -> String;

  /// The current shared fallback identity (for unauthenticated legacy clients).
  fn current_user(&self) -> RpcUser;

  /// Daemon-wide policy/backend. Required: authentication must never be
  /// acknowledged using a default no-op session setter.
  fn oauth_manager(&self) -> &Arc<OAuthManager>;

  /// Transport cancellation, also checked while a peer sends a partial frame.
  fn is_cancelled(&self) -> bool {
    false
  }

  /// How long a connection may sit without completing its handshake before
  /// the pump drops it, freeing its slot. Probes that open the endpoint and
  /// go silent (a filesystem `exists()` on Windows named pipes opens the
  /// pipe) would otherwise hold a slot forever.
  fn handshake_deadline(&self) -> Option<Duration> {
    Some(HANDSHAKE_DEADLINE)
  }

  /// Downstream sink for validated commands.
  fn sink(&self) -> &EventSink;

  /// Check and record an admitted publication. Refuse unknown pids past
  /// the tracking bound so every publication remains covered by cleanup.
  fn admit_published_pid(&mut self, pid: u64) -> bool;

  /// Whether a real pid has already been published on this connection.
  fn owns_published_pid(&self, pid: u64) -> bool;

  /// Forget a pid after an explicit clear, without granting new ownership.
  fn forget_published_pid(&mut self, pid: u64);

  /// Drain tracked pids for disconnect clears, oldest first.
  fn take_published_pids(&mut self) -> Vec<u64>;

  /// Forward one command downstream, returning whether it was queued.
  /// A shed publication must not grant ownership of a new pid.
  fn send_event(&self, cmd: ActivityCmd) -> bool {
    self.sink().emit_checked(cmd)
  }
}

/// Discord IPC packet types (little-endian `u32` header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
  /// `0`: version + client_id handshake.
  Handshake,
  /// `1`: activity command frame.
  Frame,
  /// `2`: close (emits the clear, resets state).
  Close,
  /// `3`: ping (answered with pong).
  Ping,
  /// `4`: pong.
  Pong,
}

/// Maximum IPC frame payload in bytes, matching arRPC/Discord (1 MiB).
/// Larger frames are refused with a `1003` close instead of being read.
pub const MAX_IPC_PAYLOAD: u32 = 1024 * 1024;

/// Cap on tracked pids per connection. New pids past the bound are refused,
/// not evicted: forgetting a live publication would leave a ghost card.
pub(crate) const MAX_TRACKED_PIDS: usize = 16;

/// Check and record a published pid for disconnect-clear coverage.
/// Known pids refresh as most recent; unknown pids fit only with room.
pub fn admit_pid(history: &mut Vec<u64>, pid: u64) -> bool {
  if let Some(pos) = history.iter().position(|known| *known == pid) {
    history.remove(pos);
    history.push(pid);
    true
  } else if history.len() >= MAX_TRACKED_PIDS {
    false
  } else {
    history.push(pid);
    true
  }
}

/// Legacy evicting history helper. Prefer [`admit_pid`], which refuses new
/// pids rather than losing disconnect-clear coverage for older ones.
pub fn track_pid(history: &mut Vec<u64>, pid: u64) {
  if !admit_pid(history, pid) {
    history.remove(0);
    history.push(pid);
  }
}

impl PacketType {
  /// `None` for out-of-range types: refuse with a `1003 Unsupported` close.
  #[must_use]
  pub fn try_from_u32(value: u32) -> Option<Self> {
    match value {
      0 => Some(PacketType::Handshake),
      1 => Some(PacketType::Frame),
      2 => Some(PacketType::Close),
      3 => Some(PacketType::Ping),
      4 => Some(PacketType::Pong),
      _ => None,
    }
  }
}

/// Client handshake body: protocol version plus the routing id.
#[derive(serde::Deserialize, serde::Serialize)]
pub struct Handshake {
  /// Must be `1`; anything else is refused with a `4004` close.
  pub v: u32,
  /// Application id; empty is refused with a `4000` close.
  pub client_id: String,
}

/// Encode one frame: `u32` type + `u32` length + body (all little-endian).
#[must_use]
pub fn encode(r_type: PacketType, data: &str) -> Vec<u8> {
  let mut buffer = Vec::with_capacity(8 + data.len());
  buffer.extend_from_slice(&(r_type as u32).to_le_bytes());
  buffer.extend_from_slice(&(data.len() as u32).to_le_bytes());
  buffer.extend_from_slice(data.as_bytes());
  buffer
}

/// Encode a `Close` frame carrying a Discord-style `{code, message}` body.
#[must_use]
pub fn close_frame(code: u16, message: &str) -> Vec<u8> {
  encode(
    PacketType::Close,
    &serde_json::json!({ "code": code, "message": message }).to_string(),
  )
}

/// Queue a presence clear for `pid`.
///
/// The bridge only routes `SET_ACTIVITY` into presence cleanup.
pub fn send_empty(sink: &EventSink, pid: u64) {
  let activity = ActivityCmd {
    cmd: std::borrow::Cow::Borrowed("SET_ACTIVITY"),
    args: Some(ActivityCmdArgs {
      activity: Some(None),
      code: None,
      user_id: None,
      guild_id: None,
      channel_id: None,
      pid: Some(pid),
      extra: Default::default(),
    }),
    ..ActivityCmd::empty()
  };
  sink.emit_clear(activity);
}

const MESSAGE_BUFFER_FLOOR: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Bound on how long a connection may stay un-handshaked. Generous for
/// every real client, which sends the handshake immediately after connect.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);

fn release_message_capacity(message: &mut String) {
  message.clear();
  if message.capacity() > MESSAGE_BUFFER_FLOOR {
    message.shrink_to(MESSAGE_BUFFER_FLOOR);
  }
}

struct PendingOAuth {
  task: OAuthTask,
  // Keep only the reply envelope, never a second copy of codes/tokens/args.
  reply: ActivityCmd,
}

/// A cache-miss REST fetch running off the pump thread.
struct PendingRest {
  receiver: Receiver<Result<Arc<Value>, OAuthError>>,
  reply: ActivityCmd,
}

/// Daemon-wide cap on REST fetches started by the IPC pump: cache misses
/// beyond it are refused instead of queueing more blocking work.
const REST_FETCH_LIMIT: usize = 4;
static REST_FETCHES: AtomicUsize = AtomicUsize::new(0);

/// Reserve one REST fetch slot, returning its guard, or `None` when the pump
/// should refuse.
fn try_acquire_rest_fetch() -> Option<RestFetchSlot> {
  REST_FETCHES
    .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
      (current < REST_FETCH_LIMIT).then_some(current + 1)
    })
    .ok()?;
  Some(RestFetchSlot)
}

/// Releases the slot when the detached fetch thread exits.
struct RestFetchSlot;

impl Drop for RestFetchSlot {
  fn drop(&mut self) {
    REST_FETCHES.fetch_sub(1, Ordering::AcqRel);
  }
}

#[derive(Default)]
struct Connection {
  oauth: Option<OAuthConnection>,
  pending: Option<PendingOAuth>,
  pending_rest: Option<PendingRest>,
}

impl Connection {
  fn close(&mut self) {
    if let Some(oauth) = &self.oauth {
      oauth.close();
    }
    self.pending = None;
    self.pending_rest = None;
  }

  fn poll_oauth(&mut self) -> Option<String> {
    let pending = self.pending.as_ref()?;
    let result = pending.task.try_result()?;
    let response = match result {
      Ok(result) => commands::oauth_response(&pending.reply, &result.data),
      Err(error) => auth_error(&pending.reply, &error),
    };
    self.pending = None;
    Some(response)
  }

  fn poll_rest(&mut self) -> Option<String> {
    let pending = self.pending_rest.as_ref()?;
    let result = match pending.receiver.try_recv() {
      Ok(result) => result,
      Err(TryRecvError::Empty) => return None,
      Err(TryRecvError::Disconnected) => Err(OAuthError::WorkerUnavailable),
    };
    let response = match result {
      Ok(data) => commands::rpc_data_response(&pending.reply, &data),
      Err(error) => auth_error(&pending.reply, &error),
    };
    self.pending_rest = None;
    Some(response)
  }
}

impl Drop for Connection {
  fn drop(&mut self) {
    self.close();
  }
}

/// Pump one connection until close, error, cancellation or peer loss.
///
/// The caller must configure nonblocking I/O (or bounded read/write timeouts).
/// Run on a dedicated thread or in `spawn_blocking`, never on an async worker.
/// Partial headers, bodies and replies survive WouldBlock/TimedOut. OAuth
/// workers use a separate bounded pool; waiting for them never blocks reads.
pub fn handle_stream(ipc: &mut dyn IpcFacilitator, stream: &mut (impl Read + Write)) {
  let mut decoder = Decoder::default();
  let mut replies = Replies::default();
  let mut connection = Connection::default();
  let mut closing = false;
  let mut clean_close = false;
  let connected_at = std::time::Instant::now();
  loop {
    if ipc.is_cancelled() {
      break;
    }
    // Probes that connect and go silent must not hold a slot forever.
    if !ipc.handshake()
      && let Some(deadline) = ipc.handshake_deadline()
      && connected_at.elapsed() >= deadline
    {
      tracing::debug!("[ipc] Handshake deadline elapsed, closing connection");
      break;
    }
    let mut progressed = false;
    if !closing {
      match decoder.poll_read(stream) {
        Ok(ReadFrame::Pending) => {}
        Ok(ReadFrame::Eof) => break,
        Ok(ReadFrame::Frame(packet, mut message)) => {
          progressed = true;
          let reply = match packet {
            PacketType::Handshake => match on_handshake(ipc, &mut connection, &message) {
              Ok(response) => replies.push(PacketType::Frame, &response),
              Err((code, reason)) => {
                closing = true;
                replies.push_encoded(close_frame(code, reason))
              }
            },
            PacketType::Frame => match on_frame(ipc, &mut connection, &message) {
              Some(response) => replies.push(PacketType::Frame, &response),
              None => Ok(()),
            },
            PacketType::Close => {
              clean_close = true;
              break;
            }
            PacketType::Ping => replies.push(PacketType::Pong, &message),
            PacketType::Pong => Ok(()),
          };
          if reply.is_err() {
            tracing::debug!("[ipc] Reply queue full, closing connection");
            break;
          }
          release_message_capacity(&mut message);
          decoder.recycle(message);
        }
        Err(error) => {
          let reason = match error {
            DecodeError::TooLarge => "Payload too large",
            DecodeError::UnsupportedPacket => "Unsupported packet type",
            DecodeError::InvalidUtf8 => "Invalid encoding",
            DecodeError::Io(error) => {
              tracing::debug!(kind = ?error.kind(), "[ipc] Read failed, closing connection");
              break;
            }
          };
          closing = true;
          if replies.push_encoded(close_frame(1003, reason)).is_err() {
            break;
          }
        }
      }
    }
    if closing {
      // Still try to deliver pending OAuth/REST replies before closing:
      // the peer may have shut down its read half but the worker result
      // already arrived and must reach the connection before it drops.
      let response = connection.poll_oauth().or_else(|| connection.poll_rest());
      if let Some(response) = response {
        if replies.push(PacketType::Frame, &response).is_err() {
          break;
        }
        progressed = true;
      }
      connection.close();
    } else {
      let response = connection.poll_oauth().or_else(|| connection.poll_rest());
      if let Some(response) = response {
        if replies.push(PacketType::Frame, &response).is_err() {
          break;
        }
        progressed = true;
      }
    }
    match replies.poll_flush(stream) {
      Ok(written) => progressed |= written,
      Err(error) => {
        tracing::debug!(kind = ?error.kind(), "[ipc] Reply failed, closing connection");
        break;
      }
    }
    if closing && replies.is_empty() {
      break;
    }
    if !progressed {
      std::thread::sleep(POLL_INTERVAL);
    }
  }
  connection.close();
  let clean = clean_close || ipc.is_cancelled();
  cleanup(ipc, clean);
}

fn on_handshake(
  ipc: &mut dyn IpcFacilitator,
  connection: &mut Connection,
  message: &str,
) -> Result<String, (u16, &'static str)> {
  // Even an identical handshake is a protocol violation. Never rebind an
  // authenticated connection to a new application or retain its old session.
  if connection.oauth.is_some() || ipc.handshake() {
    return Err((4000, "Already handshaked"));
  }
  let data = serde_json::from_str::<Handshake>(message).map_err(|_| (4000, "Invalid handshake"))?;
  if data.v != 1 {
    return Err((4004, "Invalid version"));
  }
  if data.client_id.is_empty() {
    return Err((4000, "Invalid client_id"));
  }
  let client_id = AppId::from(data.client_id.as_str());
  connection.oauth = Some(ipc.oauth_manager().connection(client_id.clone()));
  ipc.set_client_id(client_id);
  ipc.set_handshake(true);
  tracing::info!("[ipc] Client connected");
  Ok(ipc.user_payload())
}

/// All exit paths share cleanup. Only published pids may clear real cards;
/// never synthesize a clear for the most recently *refused* or foreign pid.
fn cleanup(ipc: &mut dyn IpcFacilitator, clean_close: bool) {
  let current = ipc.pid();
  let pids = ipc.take_published_pids();
  let full_clear = clean_close && ipc.handshake();
  if full_clear {
    let pid = if pids.contains(&current) { current } else { 0 };
    ipc.sink().emit_clear(ActivityCmd {
      application_id: Some(ipc.client_id()),
      cmd: std::borrow::Cow::Borrowed("SET_ACTIVITY"),
      args: Some(ActivityCmdArgs {
        pid: Some(pid),
        activity: Some(None),
        code: None,
        user_id: None,
        guild_id: None,
        channel_id: None,
        extra: Default::default(),
      }),
      nonce: ipc.nonce(),
      ..ActivityCmd::empty()
    });
  }
  for pid in pids {
    if !full_clear || pid != current {
      send_empty(ipc.sink(), pid);
    }
  }
  ipc.set_handshake(false);
  ipc.set_client_id(AppId::from(""));
  ipc.set_pid(0);
  ipc.set_nonce(Value::Null);
}

fn auth_error(command: &ActivityCmd, error: &OAuthError) -> String {
  let (code, message) = error.rpc_error();
  commands::rpc_error(&command.cmd, &command.nonce, code, message)
}

/// Dispatch a command, returning an immediate reply or leaving it in the
/// connection's OAuth mailbox. No command handler writes to the stream.
fn on_frame(
  ipc: &mut dyn IpcFacilitator,
  connection: &mut Connection,
  message: &str,
) -> Option<String> {
  // The handle is created only by a successful, first handshake. A stale
  // facilitator flag alone cannot grant command access.
  let oauth = connection.oauth.as_ref()?;
  let mut command = match serde_json::from_str::<ActivityCmd>(message) {
    Ok(command) => command,
    Err(_) => {
      // Serde errors may contain client-supplied values, including tokens.
      tracing::warn!("[ipc] Invalid command encoding");
      return Some(commands::rpc_error(
        "",
        &Value::Null,
        4005,
        "Invalid encoding",
      ));
    }
  };
  let owned_clear = command.cmd == "SET_ACTIVITY"
    && command.args.as_ref().is_some_and(|args| {
      matches!(args.activity, Some(None))
        && args
          .pid
          .is_some_and(|pid| pid != 0 && ipc.owns_published_pid(pid))
    });
  if let Err(error) = oauth.check_command(&command, owned_clear) {
    return Some(auth_error(&command, &error));
  }
  Some(match command.cmd.as_ref() {
    "SUBSCRIBE" | "UNSUBSCRIBE" => commands::subscribe_ack(&command),
    "SET_ACTIVITY" => {
      // Legacy unauthenticated clients retain explicit-clear compatibility.
      // Protected connections cannot clear another connection's pid, even
      // after authenticating; a null activity never establishes ownership.
      if command
        .args
        .as_ref()
        .is_some_and(|args| matches!(args.activity, Some(None)))
        && !owned_clear
      {
        let protected = match oauth.session() {
          Ok(session) => {
            session.is_some() || ipc.oauth_manager().policy() == RpcAuthPolicy::Required
          }
          Err(error) => return Some(auth_error(&command, &error)),
        };
        if protected {
          return Some(commands::rpc_error(
            &command.cmd,
            &command.nonce,
            4006,
            "Activity pid is not owned by this connection",
          ));
        }
      }
      handle_set_activity(ipc, command)
    }
    "GET_USER" => {
      let (has_session, user) = match oauth.session_user() {
        Ok((true, user)) => (true, user),
        Ok((false, _)) => (false, None),
        Err(error) => return Some(auth_error(&command, &error)),
      };
      let user = if has_session {
        user
      } else {
        Some(ipc.current_user())
      };
      let wanted = command.args.as_ref().and_then(|args| args.user_id.as_ref());
      let matched = wanted.is_none_or(|id| user.as_ref().is_some_and(|user| *id == user.id));
      commands::user_response(&command, if matched { user.as_ref() } else { None })
    }
    "GET_GUILDS" => match oauth.auth_scope("guilds") {
      Ok(Some(true)) => {
        if let Some(data) = oauth.cached_value("/users/@me/guilds") {
          commands::rpc_data_response(&command, &data)
        } else if connection.pending_rest.is_some() {
          commands::rpc_error(
            &command.cmd,
            &command.nonce,
            5000,
            "Too many concurrent Discord queries; retry later",
          )
        } else {
          match try_acquire_rest_fetch() {
            Some(slot) => {
              // Cache miss: fetch on a detached thread so the pump keeps
              // reading frames, answering pings and honouring cancellation.
              let (sender, receiver) = std::sync::mpsc::sync_channel(1);
              let fetch = oauth.clone();
              std::thread::spawn(move || {
                let _slot = slot;
                let _ = sender.send(fetch.cached_fetch("/users/@me/guilds"));
              });
              connection.pending_rest = Some(PendingRest {
                receiver,
                reply: ActivityCmd {
                  cmd: command.cmd.clone(),
                  nonce: command.nonce.clone(),
                  ..ActivityCmd::empty()
                },
              });
              return None;
            }
            None => commands::rpc_error(
              &command.cmd,
              &command.nonce,
              5000,
              "Too many concurrent Discord queries; retry later",
            ),
          }
        }
      }
      // Authenticated but missing the required scope.
      Ok(Some(false)) => commands::rpc_error(
        &command.cmd,
        &command.nonce,
        4006,
        "Missing required scope: guilds",
      ),
      Ok(None) => auth_error(&command, &OAuthError::NotAuthenticated),
      Err(error) => auth_error(&command, &error),
    },
    // GET_GUILD / GET_CHANNELS / GET_CHANNEL require the local Discord client
    // (or a bot token) — user OAuth tokens cannot access /guilds/{id}.
    "AUTHORIZE" | "AUTHENTICATE" => {
      // Keep the mailbox until its reply is consumed, even if the worker
      // finished between polls. Never overwrite a pending request's nonce.
      if connection.pending.is_some() {
        return Some(auth_error(&command, &OAuthError::OperationInProgress));
      }
      let reply = ActivityCmd {
        cmd: command.cmd.clone(),
        nonce: command.nonce.clone(),
        ..ActivityCmd::empty()
      };
      match oauth.start(command) {
        Ok(task) => {
          connection.pending = Some(PendingOAuth { task, reply });
          return None;
        }
        Err(error) => auth_error(&reply, &error),
      }
    }
    "CONNECTIONS_CALLBACK" => commands::rpc_error(
      &command.cmd,
      &command.nonce,
      1000,
      "CONNECTIONS_CALLBACK is not supported",
    ),
    "INVITE_BROWSER" | "GUILD_TEMPLATE_BROWSER" | "GIFT_CODE_BROWSER" | "DEEP_LINK" => {
      command.application_id = Some(ipc.client_id());
      let response = commands::generic_ack(&command);
      ipc.send_event(command);
      response
    }
    other => {
      let (code, message) =
        commands::unsupported_command(other).unwrap_or((1000, "Unknown command"));
      commands::rpc_error(&command.cmd, &command.nonce, code, message)
    }
  })
}

/// Validate, forward and echo the same fixed activity shape.
fn handle_set_activity(ipc: &mut dyn IpcFacilitator, mut command: ActivityCmd) -> String {
  let Some(args) = command.args.as_ref() else {
    return commands::rpc_error(&command.cmd, &command.nonce, 4005, "Missing activity args");
  };
  if args.activity.is_none() {
    return commands::rpc_error(&command.cmd, &command.nonce, 4005, "Missing activity field");
  }
  let publish = matches!(args.activity, Some(Some(_)));
  let pid = args.pid.unwrap_or_default();
  if publish && pid == 0 {
    return commands::rpc_error(&command.cmd, &command.nonce, 4005, "Missing or invalid pid");
  }
  command.application_id = Some(ipc.client_id());
  command.fix();
  let response = commands::set_activity_response(&mut command)
    .expect("validated activity args always build a response");
  if publish {
    let already_owned = ipc.owns_published_pid(pid);
    if !ipc.admit_published_pid(pid) {
      return commands::rpc_error(
        &command.cmd,
        &command.nonce,
        5000,
        "Too many tracked activity pids",
      );
    }
    let nonce = command.nonce.clone();
    let cmd_name = command.cmd.clone();
    if ipc.send_event(command) {
      ipc.set_pid(pid);
      ipc.set_nonce(nonce);
    } else {
      if !already_owned {
        ipc.forget_published_pid(pid);
      }
      return commands::rpc_error(
        &cmd_name,
        &nonce,
        5000,
        "Activity queue is full; retry later",
      );
    }
  } else {
    // Explicit clears retry through transient queue pressure like disconnect
    // clears. Keep ownership if a clear is shed, so cleanup can retry it.
    if ipc.sink().emit_clear_checked(command) {
      ipc.forget_published_pid(pid);
    }
  }
  response
}

#[cfg(test)]
#[path = "../tests/unit/frame.rs"]
mod tests;
