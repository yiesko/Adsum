use super::*;
use crate::types::{OsName, ScannedExe};

/// End-to-end Epic-SKU invariant over the real data path. (Inline: the
/// fixtures need `ScannedEntry::from_owned` plus `build_aux_maps`.)
mod epic_sku;
mod matching;
mod pattern_index;
mod sorted_index;

/// Slim entry fixture with caller-chosen executable names.
fn entry_with_exes(id: &str, os: OsName, names: &[&str]) -> Arc<ScannedEntry> {
  Arc::new(ScannedEntry {
    id: id.into(),
    name: id.into(),
    executables: names
      .iter()
      .map(|name| ScannedExe {
        name: (*name).into(),
        os: os.clone(),
        is_launcher: false,
        arguments: None,
      })
      .collect(),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  })
}

// Single-pass native+proton collection. The fused walk preserves
// both passes' contents, order and filters exactly (same automaton
// inputs as two walks, one normalization per exe).
/// Launcher entry fixture (the shared helper only builds non-launchers).
fn launcher_entry(id: &str) -> Arc<ScannedEntry> {
  Arc::new(ScannedEntry {
    id: id.into(),
    name: id.into(),
    executables: Box::new([ScannedExe {
      name: "setup.exe".into(),
      os: OsName::Win32,
      is_launcher: true,
      arguments: None,
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  })
}

/// Slim entry fixture with caller-chosen alias count.
fn entry_with_aliases(aliases: usize) -> Arc<ScannedEntry> {
  Arc::new(ScannedEntry {
    id: "1".into(),
    // Multi-word: the aux map's gate only admits matchable names, so a
    // single-word fixture would reserve slots the fill never uses.
    name: "how to fish".into(),
    executables: Box::new([]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: (0..aliases)
      .map(|i| format!("alias {i}").into_boxed_str())
      .collect(),
  })
}
