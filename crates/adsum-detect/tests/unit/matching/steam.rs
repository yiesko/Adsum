use super::*;

/// Authoritative AppId matches report their provenance on the hit.
/// (Inline: the fixture needs `SortedIndex::build`, which is
/// crate-private by design.)
#[test]
fn steam_app_id_hit_carries_its_source() {
  use crate::bundle::SortedIndex;
  use crate::types::DetectSource;

  let entry = Arc::new(ScannedEntry {
    id: "12345".into(),
    name: "Source Game".into(),
    executables: Box::new([]),
    steam_ids: Box::new(["12345".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let steam_map = SortedIndex::build(vec![("12345".into(), 0)]);
  let hit = match_steam_id(
    u64::from(std::process::id()),
    &mut String::new(),
    &SteamIdLookup {
      app_id: Some("12345"),
      // No executables declared, so the id stands on its own: any path does.
      process_path: "/opt/whatever",
      source: DetectSource::SteamAppId,
      steam_map: &steam_map,
      entries: &[entry],
      custom: &[],
      install_dir_corroborated: false,
    },
  )
  .expect("appid hits");
  assert_eq!(hit.source, DetectSource::SteamAppId);
}

/// The widened corroboration is deliberate on the Steam tier too: a
/// short Heroic launch corroborates a full declared name via the
/// basename fallback, and a path naming nothing stays refused.
#[test]
fn steam_app_id_corroborates_a_short_heroic_launch() {
  use crate::bundle::SortedIndex;
  use crate::types::{DetectSource, OsName, ScannedExe};

  let entry = Arc::new(ScannedEntry {
    id: "3240220".into(),
    name: "Grand Theft Auto V Enhanced".into(),
    executables: Box::new([ScannedExe {
      name: "grand theft auto v enhanced/gta5_enhanced.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new(["3240220".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let steam_map = SortedIndex::build(vec![("3240220".into(), 0)]);
  let hit = match_steam_id(
    u64::from(std::process::id()),
    &mut String::new(),
    &SteamIdLookup {
      app_id: Some("3240220"),
      process_path: "/s:/heroic/gtavenhanced/gta5_enhanced.exe",
      source: DetectSource::SteamAppId,
      steam_map: &steam_map,
      entries: &[Arc::clone(&entry)],
      custom: &[],
      install_dir_corroborated: false,
    },
  )
  .expect("the short Heroic launch corroborates");
  assert_eq!(hit.source, DetectSource::SteamAppId);
  assert!(
    match_steam_id(
      u64::from(std::process::id()),
      &mut String::new(),
      &SteamIdLookup {
        app_id: Some("3240220"),
        process_path: "/s:/heroic/gtavenhanced/playgtav.exe",
        source: DetectSource::SteamAppId,
        steam_map: &steam_map,
        entries: &[Arc::clone(&entry)],
        custom: &[],
        install_dir_corroborated: false,
      },
    )
    .is_none(),
    "a path naming nothing declared must stay refused"
  );
}

/// The install-dir evidence corroborates a path that names no declared
/// executable, so a demo binary absent from the database still matches.
#[test]
fn install_dir_corroboration_accepts_an_undeclared_executable() {
  use crate::bundle::SortedIndex;
  use crate::types::{DetectSource, OsName, ScannedExe};

  let entry = Arc::new(ScannedEntry {
    id: "2082470".into(),
    name: "VEIN".into(),
    executables: Box::new([ScannedExe {
      name: "win64/vein.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new(["2082470".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let steam_map = SortedIndex::build(vec![("2082470".into(), 0)]);
  let hit = match_steam_id(
    u64::from(std::process::id()),
    &mut String::new(),
    &SteamIdLookup {
      app_id: Some("2082470"),
      process_path: "/s:/steamapps/common/veindemo/vein/binaries/win64/veindemo-win64-test.exe",
      source: DetectSource::SteamAppId,
      steam_map: &steam_map,
      entries: &[entry],
      custom: &[],
      install_dir_corroborated: true,
    },
  )
  .expect("the install dir corroborates the undeclared demo executable");
  assert_eq!(hit.entry.name, "VEIN".into());
}

/// Prerequisite installers sit under the game's install dir but Steam
/// runs them with its AppId: the install-dir route must refuse them.
#[test]
fn install_dir_corroboration_refuses_prerequisite_installers() {
  use crate::bundle::SortedIndex;
  use crate::types::{DetectSource, OsName, ScannedExe};

  let entry = Arc::new(ScannedEntry {
    id: "2082470".into(),
    name: "VEIN".into(),
    executables: Box::new([ScannedExe {
      name: "win64/vein.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new(["2082470".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let steam_map = SortedIndex::build(vec![("2082470".into(), 0)]);
  let lookup = |process_path| SteamIdLookup {
    app_id: Some("2082470"),
    process_path,
    source: DetectSource::SteamAppId,
    steam_map: &steam_map,
    entries: std::slice::from_ref(&entry),
    custom: &[],
    install_dir_corroborated: true,
  };
  for path in [
    "/s:/steamapps/common/veindemo/_CommonRedist/vcredist_x64.exe",
    "/s:/steamapps/common/VeinDemo/DirectX/dxsetup.exe",
    "/s:/steamapps/common/VeinDemo/Engine/Extras/Redist/en-us/UEPrereqSetup_x64.exe",
  ] {
    assert!(
      match_steam_id(
        u64::from(std::process::id()),
        &mut String::new(),
        &lookup(path)
      )
      .is_none(),
      "{path} is a prerequisite installer, not the game"
    );
  }
  // Shelf names only count below the install dir and with a file
  // component after them: a game folder called `Redist` and an
  // executable literally named `directx` are not prerequisites.
  for path in [
    "/s:/steamapps/common/redist/game.exe",
    "/s:/steamapps/common/veindemo/bin/directx",
  ] {
    let hit = match_steam_id(
      u64::from(std::process::id()),
      &mut String::new(),
      &lookup(path),
    )
    .unwrap_or_else(|| panic!("{path} must not be read as a prerequisite"));
    assert_eq!(hit.entry.name, "VEIN".into());
  }
}

/// Without that evidence the same path stays refused: the id alone is
/// not a hit.
#[test]
fn install_dir_corroboration_false_still_refuses() {
  use crate::bundle::SortedIndex;
  use crate::types::{DetectSource, OsName, ScannedExe};

  let entry = Arc::new(ScannedEntry {
    id: "2082470".into(),
    name: "VEIN".into(),
    executables: Box::new([ScannedExe {
      name: "win64/vein.exe".into(),
      os: OsName::Win32,
      is_launcher: false,
      arguments: None,
    }]),
    steam_ids: Box::new(["2082470".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let steam_map = SortedIndex::build(vec![("2082470".into(), 0)]);
  assert!(
    match_steam_id(
      u64::from(std::process::id()),
      &mut String::new(),
      &SteamIdLookup {
        app_id: Some("2082470"),
        process_path: "/s:/steamapps/common/veindemo/vein/binaries/win64/veindemo-win64-test.exe",
        source: DetectSource::SteamAppId,
        steam_map: &steam_map,
        entries: &[entry],
        custom: &[],
        install_dir_corroborated: false,
      },
    )
    .is_none(),
    "an uncorroborated AppId must not answer"
  );
}

/// The install-dir evidence never turns a declared launcher into the
/// game: Steam launches it with the game's id.
#[test]
fn install_dir_corroboration_refuses_a_declared_launcher() {
  use crate::bundle::SortedIndex;
  use crate::types::{DetectSource, OsName, ScannedExe};

  let entry = Arc::new(ScannedEntry {
    id: "2082470".into(),
    name: "VEIN".into(),
    executables: Box::new([ScannedExe {
      name: "VeinDemo.exe".into(),
      os: OsName::Win32,
      is_launcher: true,
      arguments: None,
    }]),
    steam_ids: Box::new(["2082470".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let steam_map = SortedIndex::build(vec![("2082470".into(), 0)]);
  assert!(
    match_steam_id(
      u64::from(std::process::id()),
      &mut String::new(),
      &SteamIdLookup {
        app_id: Some("2082470"),
        process_path: "/s:/steamapps/common/veindemo/veindemo.exe",
        source: DetectSource::SteamAppId,
        steam_map: &steam_map,
        entries: &[entry],
        custom: &[],
        install_dir_corroborated: true,
      },
    )
    .is_none(),
    "a declared launcher must stay refused however the id was corroborated"
  );
}

/// An entry with no executables still stands on the id alone, with or
/// without install-dir evidence.
#[test]
fn install_dir_corroboration_keeps_id_only_entries_working() {
  use crate::bundle::SortedIndex;
  use crate::types::DetectSource;

  let entry = Arc::new(ScannedEntry {
    id: "2082470".into(),
    name: "VEIN".into(),
    executables: Box::new([]),
    steam_ids: Box::new(["2082470".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let steam_map = SortedIndex::build(vec![("2082470".into(), 0)]);
  for corroborated in [false, true] {
    let hit = match_steam_id(
      u64::from(std::process::id()),
      &mut String::new(),
      &SteamIdLookup {
        app_id: Some("2082470"),
        process_path: "/opt/whatever",
        source: DetectSource::SteamAppId,
        steam_map: &steam_map,
        entries: &[Arc::clone(&entry)],
        custom: &[],
        install_dir_corroborated: corroborated,
      },
    )
    .unwrap_or_else(|| panic!("id-only entry must hit (corroborated={corroborated})"));
    assert_eq!(hit.entry.name, "VEIN".into());
  }
}

/// A custom SKU matched through library inheritance reports the
/// library path, not a direct app-id lookup. (Inline: needs
/// `SortedIndex::build`, crate-private.)
#[test]
fn custom_sku_hit_keeps_library_source() {
  use crate::bundle::SortedIndex;
  use crate::types::DetectSource;

  let entry = Arc::new(ScannedEntry {
    id: "999".into(),
    name: "Custom Game".into(),
    executables: Box::new([]),
    steam_ids: Box::new(["999".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  });
  let empty_map = SortedIndex::build(Vec::new());
  let hit = match_steam_id(
    u64::from(std::process::id()),
    &mut String::new(),
    &SteamIdLookup {
      app_id: Some("999"),
      // Custom override with no executables: the id is the only evidence.
      process_path: "/opt/custom",
      source: DetectSource::SteamLibrary,
      steam_map: &empty_map,
      entries: &[],
      custom: &[entry],
      install_dir_corroborated: false,
    },
  )
  .expect("custom sku hits");
  assert_eq!(hit.source, DetectSource::SteamLibrary);
}
