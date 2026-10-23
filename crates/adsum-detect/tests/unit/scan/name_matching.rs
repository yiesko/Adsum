use super::*;

/// Exe-stem and folder heuristics report their own provenance.
/// (Inline: the fixtures need `SortedIndex::build`, crate-private.)
#[test]
fn name_and_folder_hits_carry_their_sources() {
  use crate::bundle::SortedIndex;
  use crate::types::DetectSource;

  let entry = Arc::new(ScannedEntry {
    id: "12345".into(),
    name: "Source Game".into(),
    executables: Box::new([]),
    steam_ids: Box::new([]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let name_map = SortedIndex::build(vec![("source game".into(), 0)]);
  let empty = SortedIndex::build(Vec::new());
  let list = [entry];
  let mut norm_out = String::new();
  let pid = u64::from(std::process::id());

  let stem_hit = match_name_or_folder(
    "/opt/source game",
    pid,
    &name_map,
    &empty,
    &list,
    &mut norm_out,
    &mut String::new(),
  )
  .expect("stem hits");
  assert_eq!(stem_hit.source, DetectSource::ExeStem);

  let folder_hit = match_name_or_folder(
    "/opt/source game/bin/run",
    pid,
    &name_map,
    &empty,
    &list,
    &mut norm_out,
    &mut String::new(),
  )
  .expect("folder hits");
  assert_eq!(folder_hit.source, DetectSource::Folder);
}
