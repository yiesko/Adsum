//! Server-level tests (unix): bind + fan-out, stale reclaim, full client
//! flow over the real socket, shutdown cleanup.

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use adsum_transport_ipc::IpcTransport;
use adsum_transport_ipc::frame::PacketType;
use adsum_types::user::RpcUser;
use common::{Scratch, read_frame, recv_cmd, write_frame};

/// Default-identity fixture shared by the bind tests.
fn user() -> std::sync::Arc<std::sync::Mutex<RpcUser>> {
  std::sync::Arc::new(std::sync::Mutex::new(RpcUser::default()))
}

/// Bind creates the socket plus fan-out links; shutdown removes both.
#[tokio::test]
async fn bind_creates_socket_and_fans_out_links() {
  let scratch = Scratch::new("bind");
  let (a, b) = (scratch.sub("a"), scratch.sub("b"));

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![a.clone(), b.clone()])
    .await
    .unwrap();
  let bound = transport.socket_path().to_string();
  assert!(bound.starts_with(a.to_string_lossy().as_ref()));
  assert!(bound.ends_with("discord-ipc-0"));
  assert!(
    std::fs::symlink_metadata(&bound)
      .expect("socket exists")
      .file_type()
      .is_socket()
  );
  // Fan-out link in the second dir points at the live socket.
  assert_eq!(
    std::fs::read_link(b.join("discord-ipc-0")).expect("fan-out link"),
    std::path::PathBuf::from(&bound)
  );

  transport.shutdown().await;
  // Shutdown cleans the socket file and our links.
  assert!(!std::path::Path::new(&bound).exists());
  assert!(std::fs::read_link(b.join("discord-ipc-0")).is_err());
}

/// The Flatpak Discord dir is bound first and every existing alias layout
/// reaches the same socket, then shutdown removes the socket and links.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preferred_dir_binds_socket_with_connectable_aliases() {
  let scratch = Scratch::new("preferred");
  let runtime = scratch.sub("runtime");
  let preferred = runtime.join("app/com.discordapp.Discord");
  std::fs::create_dir_all(&preferred).expect("preferred dir");
  let alias_layouts = [
    ".flatpak/com.discordapp.Discord/xdg-run",
    ".flatpak/com.discordapp.DiscordCanary/xdg-run",
    ".flatpak/com.discordapp.DiscordPTB/xdg-run",
    ".flatpak/dev.vencord.Vesktop/xdg-run",
    ".flatpak/org.equicord.equibop/xdg-run",
    "app/com.discordapp.DiscordCanary",
    "app/com.discordapp.DiscordPTB",
    "app/dev.vencord.Vesktop",
    "app/org.equicord.equibop",
    "snap.discord",
    "snap.discord-canary",
    "snap.discord-ptb",
  ];
  for layout in alias_layouts {
    std::fs::create_dir_all(runtime.join(layout)).expect("alias dir");
  }

  let (transport, _rx) =
    IpcTransport::bind_with_dirs(user(), vec![preferred.clone(), runtime.clone()])
      .await
      .unwrap();
  let bound = transport.socket_path().to_string();
  assert!(
    Path::new(&bound).starts_with(&preferred),
    "preferred dir must win, got {bound}"
  );
  assert!(Path::new(&bound).ends_with("discord-ipc-0"));
  assert!(
    std::fs::symlink_metadata(&bound)
      .expect("socket exists")
      .file_type()
      .is_socket()
  );

  for layout in alias_layouts {
    let link = runtime.join(layout).join("discord-ipc-0");
    assert_eq!(
      std::fs::read_link(&link).expect("alias link"),
      PathBuf::from(&bound),
      "alias {layout} must point at the bound socket"
    );
  }
  assert_eq!(
    std::fs::read_link(runtime.join("discord-ipc-0")).expect("direct alias"),
    PathBuf::from(&bound)
  );

  // A client probing the legacy Flatpak path reaches the real server.
  let mut client =
    UnixStream::connect(runtime.join(".flatpak/com.discordapp.Discord/xdg-run/discord-ipc-0"))
      .expect("connect alias");
  client
    .set_read_timeout(Some(Duration::from_secs(5)))
    .expect("timeout");
  write_frame(
    &mut client,
    PacketType::Handshake,
    r#"{"v":1,"client_id":"flatpak-game"}"#,
  );
  let (packet_type, body) = read_frame(&mut client);
  assert_eq!(packet_type, 1);
  assert!(body.contains("READY"), "expected READY, got: {body}");
  drop(client);

  transport.shutdown().await;
  assert!(!Path::new(&bound).exists());
  assert!(preferred.is_dir(), "socket dir must remain after shutdown");
  for layout in alias_layouts {
    assert!(
      std::fs::read_link(runtime.join(layout).join("discord-ipc-0")).is_err(),
      "alias {layout} must be removed on shutdown"
    );
  }
  assert!(std::fs::read_link(runtime.join("discord-ipc-0")).is_err());
}

/// Existing alias dirs are used; missing client dirs are never created.
#[tokio::test]
async fn bind_does_not_create_missing_alias_dirs() {
  let scratch = Scratch::new("alias-no-create");
  let runtime = scratch.sub("runtime");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![runtime.clone()])
    .await
    .unwrap();
  assert!(runtime.join("discord-ipc-0").exists());
  assert!(!runtime.join("app").exists());
  assert!(!runtime.join(".flatpak").exists());
  assert!(!runtime.join("snap.discord").exists());
  transport.shutdown().await;
}

/// A symlinked client layout is never used for fan-out.
#[tokio::test]
async fn symlinked_alias_dir_is_not_used() {
  let scratch = Scratch::new("alias-symlink");
  let runtime = scratch.sub("runtime");
  let outside = scratch.sub("outside");
  std::fs::create_dir_all(outside.join("com.discordapp.Discord")).expect("outside official");
  std::fs::create_dir_all(runtime.join("app")).expect("app dir");
  std::os::unix::fs::symlink(
    outside.join("com.discordapp.Discord"),
    runtime.join("app/com.discordapp.Discord"),
  )
  .expect("official symlink");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![runtime.clone()])
    .await
    .expect("bind");
  assert!(Path::new(transport.socket_path()).starts_with(&runtime));
  assert!(
    std::fs::symlink_metadata(outside.join("com.discordapp.Discord/discord-ipc-0")).is_err(),
    "fanout must not follow the symlinked client dir"
  );
  transport.shutdown().await;
  assert!(std::fs::symlink_metadata(outside.join("com.discordapp.Discord/discord-ipc-0")).is_err());
}

/// An unusable preferred dir falls through to the conventional base.
#[tokio::test]
async fn unusable_preferred_dir_falls_back_to_runtime_base() {
  let scratch = Scratch::new("preferred-unusable");
  let runtime = scratch.sub("runtime");
  let preferred = runtime.join("app/com.discordapp.Discord");
  std::fs::create_dir_all(preferred.parent().expect("app parent")).expect("app dir");
  std::fs::write(&preferred, b"squat").expect("squat preferred");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![preferred, runtime.clone()])
    .await
    .expect("fallback bind");
  assert_eq!(
    Path::new(transport.socket_path()).parent(),
    Some(runtime.as_path()),
    "must bind in the conventional base, got {}",
    transport.socket_path()
  );
  transport.shutdown().await;
}

/// Regular files squatting an index are reclaimed for the socket.
/// An unusable primary dir falls through to the next candidate instead
/// of failing the bind (a regular file is never a bindable dir, any user).
#[tokio::test]
async fn unusable_primary_dir_falls_through_to_next_candidate() {
  let scratch = Scratch::new("fallback");
  let squat = scratch.sub("a").join("not-a-dir");
  std::fs::write(&squat, b"squat").expect("squat file");
  let good = scratch.sub("b");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![squat, good.clone()])
    .await
    .expect("bind falls through");
  assert!(
    transport
      .socket_path()
      .starts_with(good.to_string_lossy().as_ref()),
    "must bind under the usable dir, got {}",
    transport.socket_path()
  );
  transport.shutdown().await;
}

/// Dropping without `shutdown()` still clears presence: the drop closes
/// live sockets (unblocking pumps parked in blocking reads), which then emit their
/// disconnect clears. The pump threads end on their own; the file cleanup
/// stays identical to `shutdown()`. Multi-thread runtime: blocking client
/// I/O must not starve the server tasks on the test thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_without_shutdown_still_clears_presence() {
  let scratch = Scratch::new("drop-clear");
  let dir = scratch.sub("a");

  let (transport, mut rx) = IpcTransport::bind_with_dirs(user(), vec![dir])
    .await
    .unwrap();
  let mut client = UnixStream::connect(transport.socket_path()).expect("connect");
  client
    .set_read_timeout(Some(Duration::from_secs(5)))
    .expect("timeout");
  write_frame(
    &mut client,
    PacketType::Handshake,
    r#"{"v":1,"client_id":"game-1"}"#,
  );
  let (packet_type, body) = read_frame(&mut client);
  assert_eq!(packet_type, 1);
  assert!(body.contains("READY"), "expected READY, got: {body}");

  write_frame(
    &mut client,
    PacketType::Frame,
    r#"{"cmd":"SET_ACTIVITY","args":{"pid":9,"activity":{"name":"G","type":0}},"nonce":"n9"}"#,
  );
  let (_, echo) = read_frame(&mut client);
  assert!(echo.contains("SET_ACTIVITY"));
  let cmd = recv_cmd(&mut rx);
  assert_eq!(cmd.args.as_ref().and_then(|a| a.pid), Some(9));

  // No shutdown: dropping must still unblock the pump and clear pid 9
  // (the pump stays parked in its read otherwise, holding the card).
  drop(transport);
  let clear = recv_cmd(&mut rx);
  assert_eq!(clear.cmd, "SET_ACTIVITY");
  assert_eq!(clear.args.as_ref().and_then(|a| a.pid), Some(9));
  assert!(
    clear
      .args
      .as_ref()
      .is_some_and(|a| matches!(a.activity, Some(None)))
  );
}

/// A foreign regular file keeps its index; bind moves to the next one.
#[tokio::test]
async fn foreign_regular_file_is_preserved_and_next_index_bound() {
  let scratch = Scratch::new("foreign-file");
  let dir = scratch.sub("a");
  let squat = dir.join("discord-ipc-0");
  std::fs::write(&squat, b"squat").expect("squat file");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![dir])
    .await
    .unwrap();
  assert!(
    transport.socket_path().ends_with("discord-ipc-1"),
    "foreign file must keep index 0, got {}",
    transport.socket_path()
  );
  assert_eq!(std::fs::read(&squat).expect("foreign file kept"), b"squat");
  assert!(
    std::fs::symlink_metadata(&squat)
      .expect("squat metadata")
      .file_type()
      .is_file()
  );
  transport.shutdown().await;
  assert_eq!(
    std::fs::read(&squat).expect("foreign file survives shutdown"),
    b"squat"
  );
}

/// A silent listener holds its index; bind moves to the next one.
#[tokio::test]
async fn live_holder_blocks_rebind_to_next_index() {
  let scratch = Scratch::new("live");
  let dir = scratch.sub("a");
  // Bound but never accepting: a successful connect is enough to prove
  // the index is held, with no PING/PONG exchange.
  let holder = UnixListener::bind(dir.join("discord-ipc-0")).expect("holder socket");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![dir.clone()])
    .await
    .unwrap();
  assert!(
    transport.socket_path().ends_with("discord-ipc-1"),
    "live index 0 must be skipped, got {}",
    transport.socket_path()
  );
  transport.shutdown().await;
  assert!(
    std::fs::symlink_metadata(dir.join("discord-ipc-0"))
      .expect("holder socket kept")
      .file_type()
      .is_socket(),
    "the live holder's path must never be removed"
  );
  drop(holder);
}

/// A dead listener's socket file is reclaimed for index 0.
#[tokio::test]
async fn stale_socket_is_reclaimed() {
  let scratch = Scratch::new("stale-reclaim");
  let dir = scratch.sub("a");
  let stale = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&stale).expect("stale socket");
  drop(listener);

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![dir])
    .await
    .unwrap();
  assert!(
    transport.socket_path().ends_with("discord-ipc-0"),
    "stale socket must be reclaimed, got {}",
    transport.socket_path()
  );
  assert!(
    std::fs::symlink_metadata(&stale)
      .expect("reclaimed socket")
      .file_type()
      .is_socket()
  );
  transport.shutdown().await;
  assert!(std::fs::symlink_metadata(&stale).is_err());
}

/// Ten held indexes fail the bind and none of them is deleted.
#[tokio::test]
async fn all_busy_indexes_return_error_without_deleting() {
  let scratch = Scratch::new("all-busy");
  let dir = scratch.sub("a");
  let mut holders = Vec::with_capacity(10);
  for index in 0..10 {
    let path = dir.join(format!("discord-ipc-{index}"));
    holders.push(UnixListener::bind(path).expect("holder socket"));
  }

  let result = IpcTransport::bind_with_dirs(user(), vec![dir.clone()]).await;
  assert!(result.is_err(), "every index is held: bind must fail");
  for index in 0..10 {
    let path = dir.join(format!("discord-ipc-{index}"));
    assert!(
      std::fs::symlink_metadata(&path)
        .expect("holder path kept")
        .file_type()
        .is_socket(),
      "index {index} must stay a socket"
    );
  }
  drop(holders);
}

/// The bound socket is published with mode 0600.
#[tokio::test]
async fn bound_socket_is_0600() {
  let scratch = Scratch::new("mode");
  let dir = scratch.sub("a");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![dir])
    .await
    .unwrap();
  let meta = std::fs::symlink_metadata(transport.socket_path()).expect("socket metadata");
  assert!(meta.file_type().is_socket());
  assert_eq!(meta.mode() & 0o777, 0o600);
  transport.shutdown().await;
}

/// NOTE: multi-thread runtime — this test does blocking client I/O on the
/// test thread while the server lives on the runtime. A `current_thread`
/// runtime would deadlock (blocked executor = stalled accept loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_client_flow_over_real_socket() {
  let scratch = Scratch::new("flow");
  let dir = scratch.sub("a");

  let (transport, mut rx) = IpcTransport::bind_with_dirs(user(), vec![dir])
    .await
    .unwrap();
  let mut client = UnixStream::connect(transport.socket_path()).expect("connect");
  client
    .set_read_timeout(Some(Duration::from_secs(5)))
    .expect("timeout");

  write_frame(
    &mut client,
    PacketType::Handshake,
    r#"{"v":1,"client_id":"game-1"}"#,
  );
  let (packet_type, body) = read_frame(&mut client);
  assert_eq!(packet_type, 1);
  assert!(body.contains("READY"), "expected READY, got: {body}");

  write_frame(
    &mut client,
    PacketType::Frame,
    r#"{"cmd":"SET_ACTIVITY","args":{"pid":9,"activity":{"name":"G","type":0}},"nonce":"n9"}"#,
  );
  let (_, echo) = read_frame(&mut client);
  assert!(echo.contains("SET_ACTIVITY"));
  let cmd = recv_cmd(&mut rx);
  assert_eq!(cmd.application_id.as_deref(), Some("game-1"));
  assert_eq!(cmd.args.as_ref().and_then(|a| a.pid), Some(9));

  // Ping round-trips without touching the sink.
  write_frame(&mut client, PacketType::Ping, "ping-1");
  let (packet_type, _) = read_frame(&mut client);
  assert_eq!(packet_type, PacketType::Pong as u32);
  assert!(rx.try_recv().is_err());

  write_frame(&mut client, PacketType::Close, "{}");
  let clear = recv_cmd(&mut rx);
  assert_eq!(clear.cmd, "SET_ACTIVITY");
  assert!(
    clear
      .args
      .as_ref()
      .is_some_and(|a| matches!(a.activity, Some(None)))
  );

  transport.shutdown().await;
}

/// A connected-but-silent peer parks its pump in a blocking read: shutdown
/// must still return promptly (closing the socket unblocks the read)
/// instead of stalling out the full drain deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_with_silent_peer_returns_promptly() {
  let scratch = Scratch::new("silent-shutdown");
  let dir = scratch.sub("a");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![dir])
    .await
    .unwrap();
  // Connect and say nothing: the pump blocks reading the first header.
  let _silent = UnixStream::connect(transport.socket_path()).expect("connect");
  tokio::time::sleep(Duration::from_millis(200)).await;

  let started = std::time::Instant::now();
  transport.shutdown().await;
  assert!(
    started.elapsed() < Duration::from_secs(4),
    "shutdown must not stall out the 5s drain deadline"
  );
}

/// Connection floods past the pump cap drop peers instead of parking
/// blocking threads without bound (Windows parity): capped busy pumps hold
/// their permits, so further peers are refused at accept and observe
/// EOF instead of READY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_past_pump_cap_drops_peers() {
  // Matches MAX_IPC_PUMPS: deliberately under the 32-thread blocking
  // pool so unrelated blocking work always has room.
  const CAP: usize = 16;
  const EXTRA: usize = 8;

  let scratch = Scratch::new("pump-cap");
  let dir = scratch.sub("a");
  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![dir])
    .await
    .unwrap();
  let path = transport.socket_path().to_string();

  // Fill every pump slot: sequential connects preserve accept order, and
  // each peer holds its pump in a blocking read afterwards.
  let mut admitted = Vec::new();
  for _ in 0..CAP {
    let mut client = UnixStream::connect(&path).expect("connect");
    client
      .set_read_timeout(Some(Duration::from_secs(5)))
      .expect("timeout");
    write_frame(
      &mut client,
      PacketType::Handshake,
      r#"{"v":1,"client_id":"game-1"}"#,
    );
    let (packet_type, body) = read_frame(&mut client);
    assert_eq!(packet_type, 1);
    assert!(body.contains("READY"));
    admitted.push(client);
  }
  // Past the cap: the server drops the socket without reading, so the
  // handshake reply never arrives. No write here on purpose: a write to
  // an already-dropped peer would panic instead of asserting. Only
  // refusal signals (EOF/reset) count as dropped: a timeout would mean
  // an admitted peer went quiet, which must fail loudly instead of
  // passing silently.
  let mut dropped = 0;
  for _ in 0..EXTRA {
    let mut client = UnixStream::connect(&path).expect("connect");
    client
      .set_read_timeout(Some(Duration::from_secs(5)))
      .expect("timeout");
    let mut header = [0_u8; 8];
    match client.read_exact(&mut header) {
      Err(e)
        if matches!(
          e.kind(),
          std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
        ) =>
      {
        dropped += 1;
      }
      Err(e) => panic!("admitted peer must answer, got read error: {e:?}"),
      Ok(()) => panic!("peer past the cap was admitted"),
    }
  }
  assert_eq!(dropped, EXTRA, "peers past the cap must be refused");
  drop(admitted);
  transport.shutdown().await;
}
