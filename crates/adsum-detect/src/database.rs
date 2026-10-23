//! Detection database ownership: the lock-free generation pointer,
//! bundle swaps, custom overrides and exclusions.
//!
//! Readers (scan ticks, EXEC classification) load the current generation
//! through [`ArcSwap`] without locking. Writers (hourly refresh, custom
//! appends) serialize on the writer lock: each reads the current
//! generation, rebuilds, and stores, so no swap ever discards a
//! concurrent edit.

use std::sync::Arc;

use super::bundle::{DetectablesBundle, build_bundle};
use super::db::{DetectableActivity, Exclusions};
use super::server::{ProcessServer, release_parse_arenas};
use super::types::ScannedEntry;

impl ProcessServer {
  /// Apply `edit` to the current custom list and swap the rebuilt bundle
  /// in one write: the read of the current generation, the edit and the
  /// store are one critical section under the writer lock, so concurrent
  /// refreshes and other custom edits can never overwrite each other
  /// from a stale snapshot. Scan readers stay lock-free (`ArcSwap`).
  fn edit_custom(&self, edit: impl FnOnce(&[Arc<ScannedEntry>]) -> Vec<Arc<ScannedEntry>>) {
    let _writer = self.writer_lock.lock().unwrap_or_else(|e| e.into_inner());
    tracing::info!(
      "[Process Scanner] Updating Aho-Corasick patterns for custom detectable activities..."
    );
    let current = self.detectables.load_full();
    // Borrow the override list instead of cloning it, and clone the main
    // list as a refcount bump rather than walking ~24k entries.
    let custom = edit(&current.custom);
    let next = match build_bundle(Arc::clone(&current.list), custom.into()) {
      Ok(next) => Arc::new(next),
      Err(e) => {
        tracing::warn!(
          "[Process Scanner] Refusing custom rebuild ({}), keeping current",
          e
        );
        return;
      }
    };
    self.detectables.store(next);
    tracing::info!("[Process Scanner] Done!");
    // Rebuilt automaton state is lean; the transient scratch is garbage.
    release_parse_arenas();
  }

  /// Replace the main detectable games database at runtime (used by the
  /// periodic refresh), rebuilding the whole bundle and swapping it in
  /// one pointer write. Returns whether the swap happened: refusals (empty
  /// input, failing build) report `false` so the refresh thread keeps its
  /// old validators and retries the data next hour instead of trusting
  /// tags for a bundle that was never installed.
  pub(crate) fn update_main_detectables(&self, detectable: Vec<DetectableActivity>) -> bool {
    // Never swap in an empty database (outage returning `[]`, corrupt
    // fetch): it would build a failing automaton and blind detection.
    // Keep serving the current data instead.
    if detectable.is_empty() {
      tracing::warn!(
        "[Process Scanner] Refusing empty detectable database update, keeping current"
      );
      return false;
    }
    tracing::info!(
      "[Process Scanner] Rebuilding Aho-Corasick patterns for main detectable activities..."
    );
    // Same writer serialization as `rebuild_custom`: appends landing in
    // this load->store window must not be discarded by the swap.
    let _writer = self.writer_lock.lock().unwrap_or_else(|e| e.into_inner());
    // Move (not clone) into the slim form, then drop the input before
    // the automata build below (~tens of MB of scratch): the fat structs
    // must not ride along to the end of this function and double the
    // rebuild peak.
    let slim: Vec<Arc<ScannedEntry>> = detectable
      .into_iter()
      .map(|entry| Arc::new(ScannedEntry::from_owned(entry)))
      .collect();
    let detectable: Arc<[Arc<ScannedEntry>]> = slim.into();
    let custom = Arc::clone(&self.detectables.load().custom);
    let next = match build_bundle(detectable, custom) {
      Ok(next) => Arc::new(next),
      Err(e) => {
        tracing::warn!(
          "[Process Scanner] Refusing detectable database update ({}), keeping current",
          e
        );
        return false;
      }
    };
    // load_full bumps the count by one transiently; subtract it back so
    // the diagnostic reports pins held by real readers.
    let old_count = Arc::strong_count(&self.detectables.load_full()).saturating_sub(1);
    let rss_before = adsum_telemetry::rss_bytes();
    self.detectables.store(next);
    let rss_after = adsum_telemetry::rss_bytes();
    tracing::info!(
      "[Process Scanner] Done! (bundle swap old_refs={} rss_before={} rss_after={})",
      old_count,
      rss_before
        .map(|b| format!("{:.1}MB", b as f64 / 1_048_576.0))
        .unwrap_or_else(|| "n/a".to_string()),
      rss_after
        .map(|b| format!("{:.1}MB", b as f64 / 1_048_576.0))
        .unwrap_or_else(|| "n/a".to_string())
    );
    // Fetch string, JSON DOM and trimmed copy are now garbage: hand the
    // hourly spike back (refresh cadence itself is unchanged).
    release_parse_arenas();
    if let Some(rss) = adsum_telemetry::rss_bytes() {
      tracing::info!(
        "[Process Scanner] post-trim rss={:.1}MB",
        rss as f64 / 1_048_576.0
      );
    }
    true
  }

  /// Stage custom entries, rebuilding the shared generation so the scan
  /// loop and diagnostics observe them.
  pub fn append_detectables(&self, detectable: Vec<DetectableActivity>) {
    // Append to the custom list, since that's what is actually scanned.
    // Full public entries convert once to the slim scanner form here,
    // moving (not cloning) their strings.
    self.edit_custom(|custom| {
      // The override list is small by construction, so copying it here is
      // fine; the ~24k-entry main list is never copied on this path.
      let mut next = Vec::with_capacity(custom.len() + detectable.len());
      next.extend(custom.iter().cloned());
      next.extend(
        detectable
          .into_iter()
          .map(|entry| Arc::new(ScannedEntry::from_owned(entry))),
      );
      next
    });
  }

  /// Drop a custom override by display name, rebuilding the generation.
  pub fn remove_detectable_by_name(&self, name: &str) {
    self.edit_custom(|custom| {
      custom
        .iter()
        .filter(|entry| {
          let current: &str = &entry.name;
          current != name
        })
        .cloned()
        .collect()
    });
  }

  /// Replace the exclusions set (startup fetch, tests). The hourly refresh
  /// thread overwrites it on the same cadence when `exclusions_url` is set.
  pub fn set_exclusions(&self, exclusions: Exclusions) {
    *self.exclusions.write().unwrap_or_else(|e| e.into_inner()) = exclusions;
  }

  /// Current detection generation, shared lock-free after the clone.
  /// Scan ticks and EXEC events each hold one Arc for their whole
  /// classification, so a concurrent refresh can only swap in the NEXT
  /// fully-built generation — never a torn mix.
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  pub fn bundle(&self) -> Arc<DetectablesBundle> {
    self.detectables.load_full()
  }
}

#[cfg(test)]
#[path = "../tests/unit/database.rs"]
mod tests;
