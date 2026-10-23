use super::*;

/// Canonical form folds separators and case: Windows library roots
/// and native process paths meet the cached `/`-separated keys.
#[test]
fn canonical_path_folds_separators_and_case() {
  assert_eq!(
    canonical_steam_path("C:\\Steam\\library"),
    "/c:/steam/library"
  );
  assert_eq!(
    canonical_steam_path("c:/Steam/Library/"),
    "/c:/steam/library/"
  );
  assert_eq!(
    canonical_steam_path("/home/u/.local/share/Steam"),
    "/home/u/.local/share/steam"
  );
  assert_eq!(canonical_steam_path("relative\\dir"), "/relative/dir");
}

/// The buffer form is byte-identical to the allocating one: it must not
/// quietly become a different canonicalization, or cached keys and
/// queries would stop matching.
#[test]
fn canonical_into_matches_allocating_form() {
  for raw in [
    "/home/u/.steam/steam/steamapps/common/Game",
    "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Game",
    "relative/path",
    "ALREADY/LOWER",
    "MiXeD\\\\Case/Path",
    "/leading",
    "trailing/",
    "",
    "日本語/パス",
    "École/LÉGENDE",
  ] {
    let mut buf = String::new();
    canonical_steam_path_into(raw, &mut buf);
    assert_eq!(buf, canonical_steam_path(raw), "mismatch for {raw:?}");
  }
}

/// The buffer form reuses the destination instead of allocating: same
/// buffer pointer across calls once it is warm.
#[test]
fn canonical_into_reuses_the_destination() {
  let mut buf = String::with_capacity(64);
  canonical_steam_path_into("/a/very/much/longer/path/than/before", &mut buf);
  let ptr = buf.as_ptr();
  canonical_steam_path_into("short", &mut buf);
  assert_eq!(buf.as_ptr(), ptr, "destination reallocated");
}

/// `match_prefix_into` agrees with the allocating form and reuses the
/// caller's buffer: two prefixes of different length must let the
/// longest win, exactly as before.
#[test]
fn match_prefix_into_matches_allocating_form() {
  let mut dirs = HashMap::new();
  dirs.insert("/opt/games/a/".to_string(), "111".to_string());
  dirs.insert("/opt/games/".to_string(), "222".to_string());
  let libs = SteamLibraries {
    dirs,
    ..Default::default()
  };

  let mut scratch = String::new();
  for path in [
    "/opt/games/a/game.exe",
    "/opt/games/b/game.exe",
    "/elsewhere/game.exe",
    "",
  ] {
    assert_eq!(
      libs.match_prefix_into(path, &mut scratch),
      libs.match_prefix(path),
      "mismatch for {path:?}"
    );
  }
  // Longest prefix wins, and the id borrows from the map not the scratch.
  assert_eq!(
    libs.match_prefix_into("/opt/games/a/x", &mut scratch),
    Some("111")
  );
  assert_eq!(
    libs.match_prefix_into("/opt/games/b/x", &mut scratch),
    Some("222")
  );
}
