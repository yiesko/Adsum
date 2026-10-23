use super::*;

/// The DB stores the Epic id uppercase, the cmdline carries it
/// lowercase: pinned so `from_owned` lowercases it and `epic_map`
/// wires the lowered id to the right activity index.
#[test]
fn an_epic_sku_lowercases_into_the_map() {
  let activity: DetectableActivity = serde_json::from_value(serde_json::json!({
    "id": "1421192030440263791",
    "name": "Grand Theft Auto V Enhanced",
    "hook": false,
    "executables": [{
      "name": "grand theft auto v enhanced\\gta5_enhanced.exe",
      "os": "win32",
      "is_launcher": false
    }],
    "third_party_skus": [
      {"distributor": "epic", "id": "B0CD075465C44F87BE3B505AC04A2E46"}
    ],
    "aliases": []
  }))
  .expect("fixture parses");
  let entry = Arc::new(ScannedEntry::from_owned(activity));
  assert_eq!(entry.epic_ids.len(), 1);
  assert_eq!(
    entry.epic_ids[0].as_ref(),
    "b0cd075465c44f87be3b505ac04a2e46"
  );
  let (_, epic, _) = build_aux_maps(&[entry]);
  assert_eq!(epic.get("b0cd075465c44f87be3b505ac04a2e46"), Some(&0));
}

/// A Steam-only SKU contributes nothing to the epic map.
#[test]
fn a_steam_sku_leaves_the_epic_map_empty() {
  let activity: DetectableActivity = serde_json::from_value(serde_json::json!({
    "id": "1",
    "name": "Steam Game",
    "hook": false,
    "executables": [{"name": "game.exe", "os": "win32", "is_launcher": false}],
    "third_party_skus": [{"distributor": "steam", "id": "3240220"}],
    "aliases": []
  }))
  .expect("fixture parses");
  let entry = Arc::new(ScannedEntry::from_owned(activity));
  assert!(entry.epic_ids.is_empty());
  let (_, epic, _) = build_aux_maps(&[Arc::clone(&entry)]);
  assert_eq!(epic.get("3240220"), None);
}
