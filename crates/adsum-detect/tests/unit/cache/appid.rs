use super::*;

use adsum_telemetry::QueueGauge;
/// A bare server: no detectable base, no listeners, no refresh.
fn server() -> ProcessServer {
  let (tx, _rx) = QueueGauge::pair();
  ProcessServer::new_with_custom(
    vec![],
    vec![],
    tx,
    crate::types::ProcessEventListeners::default(),
    crate::refresh::RefreshConfig::default(),
    vec![],
  )
}

/// An `Exec` carrying only the pid (the sweep's sole input).
fn exec(pid: u64) -> Exec {
  Exec {
    pid,
    path: String::new(),
    arguments: None,
  }
}

/// One memoized pid, as a live scan would leave it.
fn memoize(server: &ProcessServer, pid: u64) {
  let mut cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
  cache.insert(pid, AppIdMemo::Absent(1));
}

/// A spike then a collapse: the table must not sit at its high-water
/// bucket count for the rest of the session (`retain` alone keeps it).
#[test]
fn sweep_releases_spike_capacity_after_a_collapse() {
  let server = server();

  // Spike: well past the shrink threshold.
  let spike: Vec<u64> = (0..500u64).collect();
  for pid in &spike {
    memoize(&server, *pid);
  }
  let spiked_capacity = server
    .appid_cache
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .capacity();
  assert!(
    spiked_capacity > 64,
    "spike should have grown the table, got {spiked_capacity}"
  );

  // Collapse: most pids die, the sweep keeps only the survivors.
  let survivors: Vec<u64> = spike[..20].to_vec();
  let mut live = rustc_hash::FxHashSet::default();
  server.sweep_dead_appids(
    &survivors.iter().map(|pid| exec(*pid)).collect::<Vec<_>>(),
    &mut live,
  );

  let cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
  assert_eq!(cache.len(), survivors.len());
  // Capacity must follow the live set, not the spike: the release is the
  // whole point, so an assertion that holds either way proves nothing.
  assert!(
    cache.capacity() <= cache.len() * 4 + 16,
    "capacity {} should have tracked {} entries, not the spike of {}",
    cache.capacity(),
    cache.len(),
    spiked_capacity
  );
  assert!(
    cache.capacity() < spiked_capacity / 2,
    "capacity {} barely moved off the spike {}",
    cache.capacity(),
    spiked_capacity
  );
}

/// Dead pids are dropped and live ones survive — the sweep's actual job,
/// which capacity trimming must never get in the way of.
#[test]
fn sweep_drops_dead_pids_and_keeps_live_ones() {
  let server = server();
  for pid in [10u64, 20, 30] {
    memoize(&server, pid);
  }

  let mut live = rustc_hash::FxHashSet::default();
  server.sweep_dead_appids(&[exec(10), exec(30)], &mut live);

  let cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
  assert!(cache.contains_key(&10));
  assert!(!cache.contains_key(&20), "dead pid must be dropped");
  assert!(cache.contains_key(&30));
}

/// D7: the memo holds a refcounted id, so a cache hit is a refcount
/// bump instead of a `String` clone.
///
/// Every monitored game paid one `String` copy per scan tick — the
/// `.cloned()` of the memo plus the copy into the return value. With
/// `AppId` inside, both are counter bumps over the same buffer.
///
/// The invariant is pointer identity: a clone that copies would land
/// in a fresh allocation and this fails, while a refcount bump does not.
#[test]
fn cached_app_id_hit_shares_the_memoized_buffer() {
  let server = server();
  // Plant a verified memo directly, so the assertion is about the hit
  // path (no environment dependency on whether /proc/<pid>/environ
  // happens to carry a SteamAppId).
  let pid = 42u64;
  {
    let mut cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.insert(
      pid,
      AppIdMemo::Present(1, AppId::from("123456789012345678")),
    );
  }
  let stored_ptr = {
    let cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
    let Some(AppIdMemo::Present(_, id)) = cache.get(&pid) else {
      panic!("planted memo must be Present");
    };
    id.as_ptr()
  };

  let mut scratch = ExecScratch::default();
  let returned = server.cached_app_id(pid, &mut scratch).expect("hit");

  assert!(
    std::ptr::eq(returned.as_ptr(), stored_ptr),
    "a cache hit must share the memoized buffer, never copy it"
  );
  assert_eq!(&*returned, "123456789012345678");
}
