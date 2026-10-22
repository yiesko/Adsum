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

use adsum_protocol::error::AdsumError;
use adsum_transport_ipc::IpcTransport;
use adsum_transport_ipc::frame::PacketType;
use adsum_types::user::RpcUser;
use common::{
  Scratch, euid, file_id, marker_path, read_frame, read_marker, recv_cmd, write_frame, write_marker,
};

/// Default-identity fixture shared by the bind tests.
fn user() -> std::sync::Arc<std::sync::Mutex<RpcUser>> {
  std::sync::Arc::new(std::sync::Mutex::new(RpcUser::default()))
}

/// Bind creates the socket plus a marked alias; shutdown removes both.
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
  // Alias in the second dir points at the live socket and is marked ours.
  let alias = b.join("discord-ipc-0");
  assert_eq!(
    std::fs::read_link(&alias).expect("fan-out link"),
    PathBuf::from(&bound)
  );
  let marker = marker_path(&b, 0);
  let marker_meta = std::fs::symlink_metadata(&marker).expect("alias marker");
  assert!(marker_meta.file_type().is_file(), "marker must be a file");
  assert_eq!(marker_meta.mode() & 0o777, 0o600);
  assert_eq!(marker_meta.uid(), euid());
  let json = read_marker(&marker);
  assert_eq!(json["version"].as_u64(), Some(1));
  assert_eq!(json["owner_uid"].as_u64(), Some(u64::from(euid())));
  assert_eq!(json["alias"].as_str(), alias.to_str());
  assert_eq!(json["target"].as_str(), Some(bound.as_str()));
  assert_eq!(
    (json["target_dev"].as_u64(), json["target_ino"].as_u64()),
    (
      Some(file_id(Path::new(&bound)).0),
      Some(file_id(Path::new(&bound)).1)
    )
  );

  transport.shutdown().await;
  // Shutdown cleans the socket file, our alias, and its marker.
  assert!(!Path::new(&bound).exists());
  assert!(std::fs::symlink_metadata(&alias).is_err());
  assert!(std::fs::symlink_metadata(&marker).is_err());
}

/// A foreign symlink that happens to point at the bound socket is never adopted.
#[tokio::test]
async fn foreign_symlink_pointing_at_bound_socket_is_preserved() {
  let scratch = Scratch::new("foreign-bound-link");
  let (a, b) = (scratch.sub("a"), scratch.sub("b"));
  let target = a.join("discord-ipc-0");
  let alias = b.join("discord-ipc-0");
  std::os::unix::fs::symlink(&target, &alias).expect("foreign link");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![a, b.clone()])
    .await
    .unwrap();
  assert_eq!(Path::new(transport.socket_path()), target.as_path());
  assert_eq!(
    std::fs::read_link(&alias).expect("foreign link kept"),
    target
  );
  assert!(
    std::fs::symlink_metadata(marker_path(&b, 0)).is_err(),
    "a foreign link must not gain a marker"
  );

  transport.shutdown().await;
  assert_eq!(
    std::fs::read_link(&alias).expect("foreign link survives shutdown"),
    target
  );
  assert!(!target.exists(), "the owned socket itself is removed");
}

/// A live foreign alias and its marker survive bind and shutdown untouched.
#[tokio::test]
async fn shutdown_preserves_foreign_alias_and_marker() {
  let scratch = Scratch::new("foreign-live-alias");
  let (a, b, foreign) = (scratch.sub("a"), scratch.sub("b"), scratch.sub("foreign"));
  let target = foreign.join("discord-ipc-0");
  let listener = UnixListener::bind(&target).expect("foreign listener");
  let alias = b.join("discord-ipc-0");
  std::os::unix::fs::symlink(&target, &alias).expect("foreign alias");
  let marker = marker_path(&b, 0);
  write_marker(&marker, &alias, &target);
  let expected = a.join("discord-ipc-0");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![a, b.clone()])
    .await
    .unwrap();
  assert_eq!(Path::new(transport.socket_path()), expected.as_path());
  transport.shutdown().await;

  assert_eq!(
    std::fs::read_link(&alias).expect("foreign alias kept"),
    target
  );
  let json = read_marker(&marker);
  assert_eq!(json["target"].as_str(), target.to_str());
  assert_eq!(
    (json["target_dev"].as_u64(), json["target_ino"].as_u64()),
    (Some(file_id(&target).0), Some(file_id(&target).1))
  );
  drop(listener);
}

/// Recovery runs before the bind, so a dead leftover alias is replaced by one
/// that this transport owns and removes on shutdown.
#[tokio::test]
async fn abrupt_leftover_alias_is_recovered_before_bind() {
  let scratch = Scratch::new("abrupt-leftover");
  let (a, b) = (scratch.sub("a"), scratch.sub("b"));
  let stale = a.join("discord-ipc-0");
  drop(UnixListener::bind(&stale).expect("stale socket"));
  let alias = b.join("discord-ipc-0");
  std::os::unix::fs::symlink(&stale, &alias).expect("leftover alias");
  let marker = marker_path(&b, 0);
  write_marker(&marker, &alias, &stale);

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![a, b.clone()])
    .await
    .unwrap();
  let bound = PathBuf::from(transport.socket_path());
  assert_eq!(bound, stale);
  assert_eq!(
    std::fs::read_link(&alias).expect("replacement alias"),
    bound
  );
  let json = read_marker(&marker);
  assert_eq!(json["target"].as_str(), bound.to_str());
  assert_eq!(
    (json["target_dev"].as_u64(), json["target_ino"].as_u64()),
    (Some(file_id(&bound).0), Some(file_id(&bound).1))
  );

  transport.shutdown().await;
  assert!(
    std::fs::symlink_metadata(&alias).is_err(),
    "the replacement alias is owned and removed"
  );
  assert!(
    std::fs::symlink_metadata(&marker).is_err(),
    "the replacement marker is owned and removed"
  );
}

/// A marker whose target still accepts connections is a live owner: preserve it.
#[tokio::test]
async fn live_orphan_alias_is_preserved() {
  let scratch = Scratch::new("live-orphan");
  let (a, b, c) = (scratch.sub("a"), scratch.sub("b"), scratch.sub("c"));
  let target = a.join("discord-ipc-0");
  let listener = UnixListener::bind(&target).expect("live target");
  let alias = b.join("discord-ipc-0");
  std::os::unix::fs::symlink(&target, &alias).expect("orphan alias");
  let marker = marker_path(&b, 0);
  write_marker(&marker, &alias, &target);
  let expected = c.join("discord-ipc-0");

  let (transport, _rx) = IpcTransport::bind_with_dirs(user(), vec![c, b.clone()])
    .await
    .unwrap();
  assert_eq!(Path::new(transport.socket_path()), expected.as_path());
  assert_eq!(std::fs::read_link(&alias).expect("live alias kept"), target);
  let json = read_marker(&marker);
  assert_eq!(json["target"].as_str(), target.to_str());

  transport.shutdown().await;
  assert_eq!(
    std::fs::read_link(&alias).expect("live alias survives shutdown"),
    target
  );
  assert!(
    std::fs::symlink_metadata(&marker).is_ok(),
    "the live owner's marker must survive"
  );
  drop(listener);
}

/// A second start skips a live owner's alias and neither shutdown touches it.
#[tokio::test]
async fn concurrent_starts_do_not_replace_each_others_alias() {
  let scratch = Scratch::new("concurrent-starts");
  let (a, b, shared) = (scratch.sub("a"), scratch.sub("b"), scratch.sub("shared"));
  let first = a.join("discord-ipc-0");
  let (t1, _rx1) = IpcTransport::bind_with_dirs(user(), vec![a, shared.clone()])
    .await
    .unwrap();
  assert_eq!(Path::new(t1.socket_path()), first.as_path());
  let alias = shared.join("discord-ipc-0");
  let marker = marker_path(&shared, 0);
  assert_eq!(std::fs::read_link(&alias).expect("first alias"), first);
  assert!(std::fs::symlink_metadata(&marker).is_ok());

  let second = b.join("discord-ipc-0");
  let (t2, _rx2) = IpcTransport::bind_with_dirs(user(), vec![b, shared.clone()])
    .await
    .unwrap();
  assert_eq!(Path::new(t2.socket_path()), second.as_path());
  assert_eq!(
    std::fs::read_link(&alias).expect("alias kept"),
    first,
    "t2 must not replace t1's alias"
  );
  assert!(std::fs::symlink_metadata(&marker).is_ok());

  t2.shutdown().await;
  assert_eq!(
    std::fs::read_link(&alias).expect("alias survives t2"),
    first,
    "t2 must not remove t1's alias"
  );
  assert!(
    std::fs::symlink_metadata(&marker).is_ok(),
    "t2 must not remove t1's marker"
  );

  t1.shutdown().await;
  assert!(std::fs::symlink_metadata(&alias).is_err());
  assert!(std::fs::symlink_metadata(&marker).is_err());
}

/// Simultaneous binds reserve distinct indexes in the same directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_binds_reserve_distinct_indexes() {
  let scratch = Scratch::new("concurrent-bind");
  let dir = scratch.sub("a");
  let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

  let first = tokio::spawn({
    let barrier = std::sync::Arc::clone(&barrier);
    let dir = dir.clone();
    async move {
      barrier.wait().await;
      IpcTransport::bind_with_dirs(user(), vec![dir]).await
    }
  });
  let second = tokio::spawn({
    let barrier = std::sync::Arc::clone(&barrier);
    let dir = dir.clone();
    async move {
      barrier.wait().await;
      IpcTransport::bind_with_dirs(user(), vec![dir]).await
    }
  });
  let (t1, _rx1) = first.await.expect("first bind task").expect("first bind");
  let (t2, _rx2) = second
    .await
    .expect("second bind task")
    .expect("second bind");

  let paths = [
    PathBuf::from(t1.socket_path()),
    PathBuf::from(t2.socket_path()),
  ];
  let mut indexes: Vec<u8> = paths
    .iter()
    .map(|path| {
      path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("discord-ipc-"))
        .and_then(|index| index.parse().ok())
        .unwrap_or_else(|| panic!("socket index in {}", path.display()))
    })
    .collect();
  indexes.sort_unstable();
  assert_eq!(indexes, vec![0, 1], "binds must reserve distinct indexes");
  for path in &paths {
    UnixStream::connect(path).unwrap_or_else(|err| panic!("connect {}: {err}", path.display()));
  }

  t1.shutdown().await;
  t2.shutdown().await;
  let leftovers: Vec<PathBuf> = std::fs::read_dir(&dir)
    .expect("read bind dir")
    .map(|entry| entry.expect("dir entry").path())
    .collect();
  assert!(
    leftovers.is_empty(),
    "shutdown must leave no sockets or markers: {leftovers:?}"
  );
}

/// An empty bind-dir list is invalid input, never a silent `/tmp` bind.
#[tokio::test]
async fn empty_bind_dirs_return_error() {
  let err = IpcTransport::bind_with_dirs(user(), Vec::new())
    .await
    .expect_err("empty bind dirs must fail");
  match err {
    AdsumError::IpcBind { source, .. } => {
      assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
    }
    other => panic!("expected IpcBind, got {other:?}"),
  }
}

/// The Flatpak Discord dir is bound first and every existing alias layout
/// reaches the same socket, then shutdown removes the socket and links.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preferred_dir_binds_socket_with_connectable_aliases() {
  let scratch = Scratch::new("pref");
  let runtime = scratch.sub("runtime");
  let preferred = scratch.sub("runtime/app/com.discordapp.Discord");
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
    scratch.sub(&format!("runtime/{layout}"));
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
  for layout in alias_layouts {
    let marker = marker_path(&runtime.join(layout), 0);
    assert!(
      std::fs::symlink_metadata(&marker).is_ok(),
      "marker for {layout} must exist"
    );
  }
  assert!(
    std::fs::symlink_metadata(marker_path(&runtime, 0)).is_ok(),
    "marker for the direct alias must exist"
  );

  // Every published path must reach the same server: the bound socket,
  // the direct runtime alias, and each client layout alias (14 total).
  let mut paths = Vec::with_capacity(alias_layouts.len() + 2);
  paths.push(PathBuf::from(&bound));
  paths.push(runtime.join("discord-ipc-0"));
  paths.extend(
    alias_layouts
      .iter()
      .map(|layout| runtime.join(layout).join("discord-ipc-0")),
  );
  assert_eq!(paths.len(), 14, "bound path, direct alias, and 12 layouts");
  for path in &paths {
    let mut client =
      UnixStream::connect(path).unwrap_or_else(|err| panic!("connect {}: {err}", path.display()));
    client
      .set_read_timeout(Some(Duration::from_secs(5)))
      .expect("timeout");
    write_frame(
      &mut client,
      PacketType::Handshake,
      r#"{"v":1,"client_id":"flatpak-game"}"#,
    );
    let (packet_type, body) = read_frame(&mut client);
    assert_eq!(packet_type, 1, "handshake reply for {}", path.display());
    assert!(
      body.contains("READY"),
      "expected READY from {}, got: {body}",
      path.display()
    );
  }

  transport.shutdown().await;
  assert!(!Path::new(&bound).exists());
  assert!(preferred.is_dir(), "socket dir must remain after shutdown");
  for layout in alias_layouts {
    let dir = runtime.join(layout);
    assert!(
      std::fs::read_link(dir.join("discord-ipc-0")).is_err(),
      "alias {layout} must be removed on shutdown"
    );
    assert!(
      std::fs::symlink_metadata(marker_path(&dir, 0)).is_err(),
      "marker {layout} must be removed on shutdown"
    );
  }
  assert!(std::fs::read_link(runtime.join("discord-ipc-0")).is_err());
  assert!(std::fs::symlink_metadata(marker_path(&runtime, 0)).is_err());
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
  scratch.sub("outside/com.discordapp.Discord");
  scratch.sub("runtime/app");
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
  let preferred = scratch.sub("runtime/app").join("com.discordapp.Discord");
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
