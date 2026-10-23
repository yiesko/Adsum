use super::*;

/// The Epic tier's parser: the token is found case-insensitively and
/// only at an argument boundary, the value lands verbatim, and empty
/// values or glued tokens are rejected.
#[test]
fn epic_id_from_args_finds_the_token() {
  let id = "b0cd075465c44f87be3b505ac04a2e46";
  // Lowercase and mixed-case token, uppercase value (returned verbatim;
  // the caller lowercases for the map lookup).
  assert_eq!(
    epic_id_from_args(Some(&format!("game.exe -epicsandboxid={id}"))),
    Some(id)
  );
  assert_eq!(
    epic_id_from_args(Some("-EpicSandboxId=B0CD075465C44F87BE3B505AC04A2E46")),
    Some("B0CD075465C44F87BE3B505AC04A2E46")
  );
  // Value terminated by a space.
  assert_eq!(
    epic_id_from_args(Some("-epicsandboxid=b0cd -quiet")),
    Some("b0cd")
  );
  // No token at all, and no arguments.
  assert_eq!(epic_id_from_args(Some("-args -here")), None);
  assert_eq!(epic_id_from_args(None), None);
  // Empty value: reject.
  assert_eq!(epic_id_from_args(Some("-epicsandboxid= -verbose")), None);
  // Glued to a preceding word: not a token start.
  assert_eq!(epic_id_from_args(Some("launche-epicsandboxid=b0cd")), None);
}

/// One pass finds both Epic tokens; the public wrappers still expose each.
#[test]
fn epic_launch_args_reads_both_tokens_in_one_pass() {
  let args = epic_launch_args(Some("game.exe -epicsandboxid=B0CD -epicapp=APP -quiet"));
  assert_eq!(args.sandbox_id, Some("B0CD"));
  assert_eq!(args.app_name, Some("APP"));
  // Order on the command line does not matter.
  let reversed = epic_launch_args(Some("-EpicApp=APP -EpicSandboxId=B0CD"));
  assert_eq!(reversed.sandbox_id, Some("B0CD"));
  assert_eq!(reversed.app_name, Some("APP"));
  assert_eq!(epic_launch_args(None), EpicArgs::default());
}

/// The Epic app-name parser: same token rules as the sandbox id, its own
/// `-epicapp=` token, empty values and glued tokens rejected.
#[test]
fn epic_app_from_args_finds_the_token() {
  let app = "8769e24080ea413b8ebca3f1b8c50951";
  assert_eq!(
    epic_app_from_args(Some(&format!("game.exe -epicapp={app}"))),
    Some(app)
  );
  // Token matched case-insensitively; the value lands verbatim.
  assert_eq!(
    epic_app_from_args(Some("-EpicApp=8769E24080EA413B8EBCA3F1B8C50951")),
    Some("8769E24080EA413B8EBCA3F1B8C50951")
  );
  // Value terminated by a space.
  assert_eq!(
    epic_app_from_args(Some("-epicapp=8769e240 -quiet")),
    Some("8769e240")
  );
  // No token at all, and no arguments.
  assert_eq!(epic_app_from_args(Some("-epicsandboxid=b0cd")), None);
  assert_eq!(epic_app_from_args(None), None);
  // Empty value: reject.
  assert_eq!(epic_app_from_args(Some("-epicapp= -verbose")), None);
  // Glued to a preceding word: not a token start.
  assert_eq!(epic_app_from_args(Some("x-epicapp=8769e240")), None);
}

/// Epic sandbox id hit: carries its provenance. (Inline: the fixture
/// needs `SortedIndex::build`, which is crate-private by design.)
#[test]
fn match_epic_id_hit_carries_its_source() {
  use crate::bundle::SortedIndex;
  use crate::types::DetectSource;
  use crate::types::{OsName, ScannedExe};

  let entry = Arc::new(ScannedEntry {
    id: "1421192030440263791".into(),
    name: "Grand Theft Auto V Enhanced".into(),
    executables: Box::new([ScannedExe {
      name: "grand theft auto v enhanced/gta5_enhanced.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new(["b0cd075465c44f87be3b505ac04a2e46".into()]),
    aliases: Box::new([]),
  });
  let epic_map = SortedIndex::build(vec![("b0cd075465c44f87be3b505ac04a2e46".into(), 0)]);
  let hit = match_epic_id(
    u64::from(std::process::id()),
    &mut String::new(),
    "b0cd075465c44f87be3b505ac04a2e46",
    // The real Heroic path, normalized: the declared directory is
    // absent, so the bare-basename fallback corroborates.
    "/s:/heroic/gtavenhanced/gta5_enhanced.exe",
    &epic_map,
    &[entry],
    &[],
  )
  .expect("epic id hits");
  assert_eq!(hit.source, DetectSource::EpicAppId);
}

/// Epic id without the executable it declares: rejected. Heroic hands the
/// id to the launcher chain too; corroboration tells the game from it.
#[test]
fn match_epic_id_rejects_a_path_that_names_nothing_declared() {
  use crate::bundle::SortedIndex;
  use crate::types::{OsName, ScannedExe};

  let entry = Arc::new(ScannedEntry {
    id: "1421192030440263791".into(),
    name: "Grand Theft Auto V Enhanced".into(),
    executables: Box::new([ScannedExe {
      name: "grand theft auto v enhanced/gta5_enhanced.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new([]),
    epic_ids: Box::new(["b0cd075465c44f87be3b505ac04a2e46".into()]),
    aliases: Box::new([]),
  });
  let epic_map = SortedIndex::build(vec![("b0cd075465c44f87be3b505ac04a2e46".into(), 0)]);
  assert!(
    match_epic_id(
      u64::from(std::process::id()),
      &mut String::new(),
      "b0cd075465c44f87be3b505ac04a2e46",
      "/s:/heroic/gtavenhanced/playgtav.exe",
      &epic_map,
      &[entry],
      &[],
    )
    .is_none(),
    "a path that names none of the entry's executables must be refused"
  );
}
