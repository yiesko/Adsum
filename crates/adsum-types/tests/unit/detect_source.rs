use super::*;

/// The wire identifiers are a published contract (snapshot files and
/// journal correlation depend on them), so they are pinned exactly —
/// a rename must be deliberate, with a migration.
#[test]
fn wire_identifiers_are_stable() {
  let expected: [(DetectSource, &str); 8] = [
    (DetectSource::Automaton, "automaton"),
    (DetectSource::CwdJoined, "cwd-joined"),
    (DetectSource::ProtonAutomaton, "proton-automaton"),
    (DetectSource::SteamAppId, "steam-app-id"),
    (DetectSource::EpicAppId, "epic-app-id"),
    (DetectSource::SteamLibrary, "steam-library"),
    (DetectSource::ExeStem, "exe-stem"),
    (DetectSource::Folder, "folder"),
  ];
  for (source, id) in expected {
    assert_eq!(source.as_str(), id);
    assert_eq!(source.to_string(), id, "Display drifted from as_str");
  }
}
