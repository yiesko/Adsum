//! Which matcher classified a process.
//!
//! Lives here rather than in `adsum-detect` because the value crosses the
//! scanner/bridge boundary inside [`crate::bridge::ScannedGame`](),
//! and the bridge deliberately does not depend on the detector: a
//! detection-latency note would otherwise drag the whole automaton
//! builder into every bridge build.
//!
//! `Copy` and allocation-free by design — it travels on every hit without
//! touching the hot path's budget. `as_str` is the stable wire
//! identifier snapshots and logs have always carried.

/// Which matcher classified a process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DetectSource {
  /// Database-declared path (Aho-Corasick on the process path).
  Automaton,
  /// Bare exe joined with the process working directory.
  CwdJoined,
  /// `win32` (Proton) database entries.
  ProtonAutomaton,
  /// Authoritative store id (Steam/Proton environ, or cmdline fallback).
  SteamAppId,
  /// Epic sandbox id from the launch command line, or an `-epicapp` name
  /// resolved through Heroic's library to the same namespace (Heroic/Epic).
  EpicAppId,
  /// Install directory inherited from a known Steam library.
  SteamLibrary,
  /// Executable stem equal to a multi-word game name.
  ExeStem,
  /// Install folder carrying the title.
  Folder,
}

impl DetectSource {
  /// Stable log identifier for journal correlation.
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Automaton => "automaton",
      Self::CwdJoined => "cwd-joined",
      Self::ProtonAutomaton => "proton-automaton",
      Self::SteamAppId => "steam-app-id",
      Self::EpicAppId => "epic-app-id",
      Self::SteamLibrary => "steam-library",
      Self::ExeStem => "exe-stem",
      Self::Folder => "folder",
    }
  }
}

/// The wire identifier is the `Display` form, so `format!`/`{}` and
/// `as_str()` can never drift.
impl std::fmt::Display for DetectSource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

#[cfg(test)]
#[path = "../tests/unit/detect_source.rs"]
mod tests;
