use super::*;

/// The pre-index algorithm, verbatim — the parity oracle, kept dumb.
/// The index must answer identically on every input.
fn linear_tail(dirs: &HashMap<String, String>, canonical: &str) -> Option<String> {
  if has_dotdot_component(canonical) {
    return None;
  }
  if canonical.contains("/steamapps/compatdata/") {
    return None;
  }
  let (_, path_tail) = canonical.split_once(STEAM_COMMON)?;
  let mut best: Option<&str> = None;
  let mut best_len = 0;
  let mut ambiguous = false;
  for (prefix, appid) in dirs {
    let Some((_, tail)) = prefix.split_once(STEAM_COMMON) else {
      continue;
    };
    let tail = tail.strip_suffix('/').unwrap_or(tail);
    if tail.is_empty() || has_dotdot_component(tail) {
      continue;
    }
    if !install_tail_names(path_tail, tail) {
      continue;
    }
    if tail.len() > best_len {
      best = Some(appid.as_str());
      best_len = tail.len();
      ambiguous = false;
    } else if tail.len() == best_len && best != Some(appid.as_str()) {
      ambiguous = true;
    }
  }
  if ambiguous {
    None
  } else {
    best.filter(|appid| !appid.is_empty()).map(str::to_string)
  }
}

/// Hand-built `dirs` (in-crate tests touch the private fields) covering
/// every shape the index must survive: multiple libraries, nested tails,
/// compatdata, the conflict sentinel, dotdot tails and non-Steam prefixes.
fn fixture() -> SteamLibraries {
  let mut libs = SteamLibraries::default();
  for (prefix, appid) in [
    ("/home/u/steam/steamapps/common/garrysmod/", "4000"),
    ("/home/u/steam/steamapps/common/portal 2/", "620"),
    ("/mnt/games/steamapps/common/portal 2/", "620"),
    // Same tail, DIFFERENT appid, same length: ambiguous by rule.
    ("/other/steamapps/common/doom/", "1"),
    ("/other2/steamapps/common/doom/", "2"),
    // Nested tail: first component indexes it, boundary rules decide.
    ("/home/u/steam/steamapps/common/illarion/jre/", "9"),
    // Proton prefix: no `common/` segment — invisible to the tier.
    ("/home/u/steam/steamapps/compatdata/4000/", "4000"),
    // Conflict sentinel: two manifests claimed one prefix.
    ("/home/u/steam/steamapps/common/contested/", ""),
    // Traversal tail: refused at build time, then and now.
    ("/home/u/steam/steamapps/common/evil/../x/", "666"),
    // Empty tail (prefix ends at `common/`): refused.
    ("/weird/steamapps/common/", "777"),
  ] {
    libs.dirs.insert(prefix.to_string(), appid.to_string());
  }
  libs.rebuild_tail_index();
  libs
}

/// The parity corpus: hits, boundary near-misses, deeper paths, case and
/// separator folding (the query side canonicalizes), compatdata refusal,
/// traversal refusal, absent installs and the empty-tail query.
#[test]
fn tail_index_answers_identically_to_the_linear_walk() {
  let libs = fixture();
  for raw in [
    "/home/u/Steam/steamapps/common/GarrysMod/hl2.exe",
    "/home/u/steam/steamapps/common/garrysmod",
    "/home/u/steam/steamapps/common/garrysmodx/hl2.exe", // near miss
    "/home/u/steam/steamapps/common/portal 2/portal2.exe",
    "/mnt/games/steamapps/common/portal 2/bin/x",
    "C:\\Other\\SteamApps\\Common\\DOOM\\doom.exe", // ambiguous tail
    "/home/u/steam/steamapps/common/illarion/jre/bin/java.exe",
    "/home/u/steam/steamapps/common/illarion/jre",
    "/home/u/steam/steamapps/common/illarion/jrex", // near miss
    "/home/u/steam/steamapps/compatdata/4000/pfx/drive_c/game.exe",
    "/home/u/steam/steamapps/common/evil/../x/game.exe",
    "/home/u/steam/steamapps/common/contested/game.exe",
    "/home/u/steam/steamapps/common/",
    "/home/u/steam/steamapps/common/absent/game.exe",
    "/no/steam/here/game.exe",
    "",
  ] {
    let mut scratch = String::new();
    let indexed = libs.match_install_tail_into(raw, &mut scratch);
    let mut canonical = String::new();
    canonical_steam_path_into(raw, &mut canonical);
    let linear = linear_tail(&libs.dirs, &canonical);
    assert_eq!(
      indexed.map(str::to_string),
      linear,
      "index disagrees with the linear walk for {raw:?}"
    );
  }
}

/// The rules the corpus pins, spelled out (a parity bug in BOTH sides
/// would pass the test above): ambiguous and conflict sentinels refuse,
/// compatdata refuses, the nested tail answers on its boundary.
#[test]
fn tail_rules_hold_explicitly() {
  let libs = fixture();
  let mut s = String::new();
  assert_eq!(
    libs.match_install_tail_into("/other/steamapps/common/doom/doom.exe", &mut s),
    None,
    "two appids claim one tail: ambiguous refuses"
  );
  assert_eq!(
    libs.match_install_tail_into("/home/u/steam/steamapps/common/contested/x.exe", &mut s),
    None,
    "the conflict sentinel answers nobody"
  );
  assert_eq!(
    libs.match_install_tail_into("/home/u/steam/steamapps/compatdata/4000/pfx/g.exe", &mut s),
    None,
    "a Proton prefix is runtime state, never an install"
  );
  assert_eq!(
    libs.match_install_tail_into(
      "/home/u/steam/steamapps/common/illarion/jre/bin/java.exe",
      &mut s
    ),
    Some("9"),
    "the nested tail answers below its boundary"
  );
  assert_eq!(
    libs.match_install_tail_into(
      "C:\\Home\\U\\Steam\\SteamApps\\Common\\GarrysMod\\hl2.exe",
      &mut s
    ),
    Some("4000"),
    "case and separator folding reach the same entry"
  );
}

/// The index is derived state and must follow `dirs` through mutations.
/// Production rebuilds it inside every `dirs` writer; this pins that.
#[test]
fn index_follows_dirs_mutations() {
  let mut libs = SteamLibraries::default();
  let mut s = String::new();
  assert_eq!(
    libs.match_install_tail_into("/l/steamapps/common/game/g.exe", &mut s),
    None
  );
  libs
    .dirs
    .insert("/l/steamapps/common/game/".to_string(), "42".to_string());
  libs.rebuild_tail_index();
  assert_eq!(
    libs.match_install_tail_into("/l/steamapps/common/game/g.exe", &mut s),
    Some("42")
  );
  libs.dirs.remove("/l/steamapps/common/game/");
  libs.rebuild_tail_index();
  assert_eq!(
    libs.match_install_tail_into("/l/steamapps/common/game/g.exe", &mut s),
    None,
    "a removed install must stop answering"
  );
}
