//! Detection provenance: stable log identifiers for journal correlation.

use adsum_detect::types::DetectSource;

/// Log strings are stable identifiers for journal correlation.
#[test]
fn detect_source_log_strings() {
  assert_eq!(DetectSource::Automaton.as_str(), "automaton");
  assert_eq!(DetectSource::CwdJoined.as_str(), "cwd-joined");
  assert_eq!(DetectSource::ProtonAutomaton.as_str(), "proton-automaton");
  assert_eq!(DetectSource::SteamAppId.as_str(), "steam-app-id");
  assert_eq!(DetectSource::EpicAppId.as_str(), "epic-app-id");
  assert_eq!(DetectSource::SteamLibrary.as_str(), "steam-library");
  assert_eq!(DetectSource::ExeStem.as_str(), "exe-stem");
  assert_eq!(DetectSource::Folder.as_str(), "folder");
}
use adsum_detect::types::{ScannedEntry, ScannedExe};

/// The immutable generation arrays are boxed: `Box<[T]>` is 16 B where
/// `Vec<T>` is 24 B and holds no excess capacity. Three fields changed, so
/// 24 B off every entry, and `epic_ids` joined as a fourth boxed slice
/// (data model, another 16 B per entry); the struct grew by 16 B overall.
/// Across ~24k entries that is a real, permanent saving worth pinning.
#[test]
fn scanned_entry_arrays_are_boxed_not_vec() {
  let entry = ScannedEntry {
    id: "1".into(),
    name: "G".into(),
    executables: Box::new([ScannedExe {
      name: "g.exe".into(),
      os: adsum_detect::types::OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new(["42".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new(["Alt".into()]),
  };
  // The exact layout: three converted slices are 16 B rather than 24 B;
  // `epic_ids` is a new 16 B slice on top, so the struct grew by 16 B.
  assert_eq!(std::mem::size_of_val(&entry.executables), 16);
  assert_eq!(std::mem::size_of_val(&entry.steam_ids), 16);
  assert_eq!(std::mem::size_of_val(&entry.epic_ids), 16);
  assert_eq!(std::mem::size_of_val(&entry.aliases), 16);
  assert!(
    std::mem::size_of::<ScannedEntry>() <= 96,
    "ScannedEntry grew to {} bytes",
    std::mem::size_of::<ScannedEntry>()
  );
  // Contents survive the boxing.
  assert_eq!(entry.executables.len(), 1);
  assert_eq!(entry.steam_ids[0].as_ref(), "42");
  assert_eq!(entry.aliases[0].as_ref(), "Alt");
}

/// `ScannedExe.arguments` is boxed like the rest of the build-once text:
/// `Option<String>` is 24 B with a spare capacity field, `Option<Box<str>>`
/// is 16 B. Only ~15 of the ~11k executables carry arguments, so this is a
/// per-entry structural saving rather than a heap one.
#[test]
fn scanned_exe_arguments_are_boxed() {
  let with_args = ScannedExe {
    name: "g.exe".into(),
    os: adsum_detect::types::OsName::Win32,
    is_launcher: false,
    arguments: Some("--flag".into()),
  };
  let without = ScannedExe {
    arguments: None,
    ..with_args.clone()
  };
  assert_eq!(with_args.arguments.as_deref(), Some("--flag"));
  assert!(without.arguments.is_none());
  assert_eq!(
    std::mem::size_of_val(&with_args.arguments),
    std::mem::size_of::<Option<Box<str>>>()
  );
  assert!(
    std::mem::size_of::<ScannedExe>() <= 64,
    "ScannedExe grew to {} bytes",
    std::mem::size_of::<ScannedExe>()
  );
}

/// The matcher's argument check reads the boxed field unchanged: a boxed
/// `str` derefs to `&str`, so `contains` still takes it as a pattern.
#[test]
fn boxed_arguments_still_match_as_a_pattern() {
  let exe = ScannedExe {
    name: "g.exe".into(),
    os: adsum_detect::types::OsName::Win32,
    is_launcher: false,
    arguments: Some("--windowed".into()),
  };
  let cmdline = "game.exe --windowed --verbose".to_string();
  assert!(cmdline.contains(&**exe.arguments.as_ref().expect("args")));
  let other = "game.exe".to_string();
  assert!(!other.contains(&**exe.arguments.as_ref().expect("args")));
}
