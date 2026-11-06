//! Whole-word argument matching and candidate selection.
//!
//! The database's `arguments` field can hold a sequence (`-game tf`).
//! Matching it as a raw substring accepts `-game tf2` because `tf` is a
//! prefix of `tf2`; the matcher must require whole-word boundaries.

use std::sync::Arc;

use crate::matching::{MatchOutcome, arguments_contain_whole_word, first_hit_naming_path};
use crate::types::{OsName, ScannedEntry, ScannedExe};

/// A lone token matches only itself, never a longer token sharing a prefix.
#[test]
fn whole_word_rejects_a_longer_token() {
  assert!(arguments_contain_whole_word(Some("-game tf"), "-game tf"));
  assert!(!arguments_contain_whole_word(Some("-game tf2"), "-game tf"));
  assert!(!arguments_contain_whole_word(
    Some("-game hl2mp"),
    "-game hl2"
  ));
}

/// Boundaries are start/end of the command line or a space on both sides.
#[test]
fn whole_word_honours_start_and_end_boundaries() {
  assert!(arguments_contain_whole_word(Some("-game tf"), "-game tf"));
  assert!(arguments_contain_whole_word(
    Some("-game tf -foo"),
    "-game tf"
  ));
  assert!(arguments_contain_whole_word(
    Some("tf -game tf"),
    "-game tf"
  ));
  assert!(!arguments_contain_whole_word(Some("x-game tf"), "-game tf"));
  assert!(!arguments_contain_whole_word(Some("-game tfx"), "-game tf"));
}

/// An empty requirement imposes no constraint (parity with the old
/// `contains("")` behavior on an empty database field).
#[test]
fn whole_word_empty_requirement_imposes_nothing() {
  assert!(arguments_contain_whole_word(Some("anything here"), ""));
  assert!(arguments_contain_whole_word(None, ""));
  assert!(!arguments_contain_whole_word(None, "x"));
}

/// The five database entries that carry required arguments still match
/// their own command line and no longer match a suffixed variant.
#[test]
fn database_entries_match_their_own_argv() {
  let cases = [
    ("-game garrysmod", "-game garrysmod"),
    ("-game tf", "-game tf"),
    ("-game left4dead2", "-game left4dead2"),
    ("-game hl2", "-game hl2"),
    (
      "net.minecraft.client.main.Main",
      "net.minecraft.client.main.Main",
    ),
  ];
  for (required, argv) in cases {
    assert!(
      arguments_contain_whole_word(Some(argv), required),
      "{required} should match {argv}"
    );
  }
  // Suffix rejection on the same five.
  assert!(!arguments_contain_whole_word(
    Some("-game garrysmodmp"),
    "-game garrysmod"
  ));
  assert!(!arguments_contain_whole_word(Some("-game tf2"), "-game tf"));
  assert!(!arguments_contain_whole_word(
    Some("-game left4dead2x"),
    "-game left4dead2"
  ));
  assert!(!arguments_contain_whole_word(
    Some("-game hl2mp"),
    "-game hl2"
  ));
  assert!(!arguments_contain_whole_word(
    Some("net.minecraft.client.main.MainKt"),
    "net.minecraft.client.main.Main"
  ));
}

/// A candidate that names the path but fails its arguments is skipped so
/// a later candidate that names the path and carries the arguments wins.
#[test]
fn argument_mismatch_skips_to_later_candidate() {
  let passing = Arc::new(ScannedEntry {
    id: "2".into(),
    name: "Passing".into(),
    executables: Box::new([ScannedExe {
      name: "game.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let failing = Arc::new(ScannedEntry {
    id: "1".into(),
    name: "Failing".into(),
    executables: Box::new([ScannedExe {
      name: "game.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: Some("-game tf".into()),
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  // The automaton yields the failing candidate first.
  let probe = vec![(Arc::clone(&failing), 0), (Arc::clone(&passing), 0)];
  let outcome = first_hit_naming_path(probe.into_iter(), "/dir/game.exe", Some("-game tf2"));
  match outcome {
    MatchOutcome::Corroborated(obj, _) => {
      assert_eq!(obj.id.as_ref(), "2", "the passing candidate wins");
    }
    other => panic!("expected Corroborated, got {other:?}"),
  }
}

/// When every candidate that names the path fails its arguments, the walk
/// ends in Refused, not NoMatch: the path did carry declared names.
#[test]
fn argument_mismatch_refuses_when_no_later_candidate() {
  let failing = Arc::new(ScannedEntry {
    id: "1".into(),
    name: "Failing".into(),
    executables: Box::new([ScannedExe {
      name: "game.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: Some("-game tf".into()),
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let probe = vec![(Arc::clone(&failing), 0)];
  let outcome = first_hit_naming_path(probe.into_iter(), "/dir/game.exe", Some("-game tf2"));
  assert!(
    matches!(outcome, MatchOutcome::Refused),
    "a path that names a declared executable but fails its arguments is Refused"
  );
}

/// A candidate that does not name the path is skipped without marking
/// the variant refused on arguments.
#[test]
fn non_naming_candidate_does_not_block_the_variant() {
  let stranger = Arc::new(ScannedEntry {
    id: "9".into(),
    name: "Stranger".into(),
    executables: Box::new([ScannedExe {
      name: "other.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let probe = vec![(Arc::clone(&stranger), 0)];
  let outcome = first_hit_naming_path(probe.into_iter(), "/dir/game.exe", Some("-game tf"));
  assert!(
    matches!(outcome, MatchOutcome::Refused),
    "a candidate whose name does not name the path is Refused, not NoMatch"
  );
}
