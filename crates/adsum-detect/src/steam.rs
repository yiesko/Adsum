//! Steam library roots: install-dir to AppId mapping (VDF provider).
//!
//! Refreshed once per scan tick when a `libraryfolders.vdf` changed and
//! consulted on path misses, so Steam games resolve without repeated
//! filesystem walks.

use adsum_steam::SteamLibraries;

use super::server::ProcessServer;

impl ProcessServer {
  /// Replace the Steam libraries map (hermetic construction seam for
  /// tests and tooling; production discovers at construction and refreshes
  /// per tick).
  pub fn set_steam_libraries(&self, libraries: SteamLibraries) {
    *self
      .steam_libraries
      .write()
      .unwrap_or_else(|e| e.into_inner()) = libraries;
  }

  /// AppId whose Steam install dir prefixes the process path, by host
  /// prefix. Host-prefix form: kept for tests and tooling that reason
  /// about a concrete library root; the scan loop uses the Proton-safe
  /// [`steam_install_tail_app_id_into`](Self::steam_install_tail_app_id_into).
  /// Cloned out of the lock; tiny strings.
  ///
  /// Allocates for the canonical form: prefer
  /// [`steam_prefix_app_id_into`](Self::steam_prefix_app_id_into) when
  /// reusing a buffer.
  pub fn steam_prefix_app_id(&self, normalized_path: &str) -> Option<String> {
    self
      .steam_libraries
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .match_prefix(normalized_path)
      .map(str::to_string)
  }

  /// Same host-prefix match, canonicalizing into a caller-owned buffer.
  /// Tests/tooling only: the scan loop uses the tail variant, which also
  /// reaches Proton paths whose library root was rewritten.
  pub fn steam_prefix_app_id_into(
    &self,
    normalized_path: &str,
    scratch: &mut String,
  ) -> Option<String> {
    self
      .steam_libraries
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .match_prefix_into(normalized_path, scratch)
      .map(str::to_string)
  }

  /// AppId whose `steamapps/common/<installdir>/` tail prefixes the path:
  /// the Proton-safe install-dir evidence, without the host prefix.
  pub fn steam_install_tail_app_id_into(
    &self,
    normalized_path: &str,
    scratch: &mut String,
  ) -> Option<String> {
    self
      .steam_libraries
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .match_install_tail_into(normalized_path, scratch)
      .map(str::to_string)
  }

  /// Whether the path sits under `app_id`'s own install dir. Compares the
  /// matched id under the lock, so the common path clones nothing.
  pub fn steam_install_tail_matches_app_id_into(
    &self,
    normalized_path: &str,
    app_id: &str,
    scratch: &mut String,
  ) -> bool {
    self
      .steam_libraries
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .match_install_tail_into(normalized_path, scratch)
      .is_some_and(|library_appid| library_appid == app_id)
  }

  /// Revalidate the Steam libraries (stats only unless something changed).
  /// Called once per scan tick; the scan loop goes through here so tests
  /// can drive the same path.
  pub fn refresh_steam_libraries(&self) {
    self
      .steam_libraries
      .write()
      .unwrap_or_else(|e| e.into_inner())
      .refresh_if_stale();
  }
}
