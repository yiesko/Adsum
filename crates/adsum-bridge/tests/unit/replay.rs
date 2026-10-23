use super::*;

/// The replay cache never outgrows its bound, no matter how many
/// distinct publishers arrive (memory regression net: steady-state
/// RSS must not ratchet with publisher count).
#[test]
fn prune_keeps_cache_within_bound() {
  use adsum_protocol::commands::empty_cached;
  use adsum_types::SocketId;

  let mut cache: ReplayCache = HashMap::new();
  for i in 0..(crate::config::MAX_CACHED_ACTIVITIES * 3) {
    let id = SocketId::from(i.to_string());
    let payload = empty_cached(i as u64, id.clone()).expect("fixed shapes build");
    cache.insert(id, (payload, i as u64));
  }
  prune_cache(&mut cache);
  assert!(
    cache.len() <= crate::config::MAX_CACHED_ACTIVITIES,
    "cache must stay bounded, got {}",
    cache.len()
  );
}
