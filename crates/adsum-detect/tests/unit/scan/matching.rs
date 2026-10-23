use super::*;

/// Whether the process path names an executable the entry declares.
///
/// A path suffix on a component boundary: the database's names are
/// relative (`hl2.exe`, `garrysmod/hl2.exe`, `win64/gmod.exe`) and a
/// Proton `argv[0]` is a full Windows path with them at the end.
#[test]
fn argv_names_an_executable_matches_on_a_component_boundary() {
  use crate::types::{OsName, ScannedExe};

  let entry = ScannedEntry {
    id: "1".into(),
    name: "Garry's Mod".into(),
    executables: Box::new([
      ScannedExe {
        name: "hl2.exe".into(),
        os: OsName::Win32,
        is_launcher: false,
        arguments: None,
      },
      ScannedExe {
        name: "garrysmod/hl2.exe".into(),
        os: OsName::Win32,
        is_launcher: false,
        arguments: None,
      },
    ]),
    steam_ids: Box::new(["4000".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  };

  for path in [
    // Full Proton path: the declared name sits at the end.
    "/C:/Steam/steamapps/common/GarrysMod/garrysmod/hl2.exe",
    "/C:/Steam/steamapps/common/GarrysMod/hl2.exe",
    // The bare relative name, as the database spells it.
    "hl2.exe",
    "garrysmod/hl2.exe",
    // Case does not matter: `argv[0]` arrives lowercased.
    "/C:/Steam/steamapps/common/garrysmod/HL2.EXE",
  ] {
    assert!(
      argv_names_an_executable(path, &entry),
      "{path} should name a declared executable"
    );
  }

  for path in [
    // A near miss is not a match: the boundary is a `/`, not any byte.
    "/C:/Steam/steamapps/common/GarrysMod/xhl2.exe",
    "/C:/Steam/steamapps/common/GarrysMod/hl2.exe.txt",
    "/C:/Steam/steamapps/common/GarrysMod/sub/gmad.exe",
    "/C:/Steam/steamapps/common/GarrysMod/hl2.exe.evil",
    // A different game entirely.
    "/C:/Steam/steamapps/common/deadlock/game/bin/win64/deadlock.exe",
  ] {
    assert!(
      !argv_names_an_executable(path, &entry),
      "{path} must not pass for a declared executable"
    );
  }
}

/// The premise the platform-aware provenance assertion rests on: an
/// executable's `os` tag decides which automaton — if any — carries its
/// pattern. `win32` is native on Windows and Proton-only elsewhere, so
/// on Windows the automaton runs before the store-id tier and elsewhere
/// it runs after it, or never. Pinned here because the assertion that
/// depends on it cannot run the other two platforms locally.
#[test]
fn os_matches_only_the_running_platform() {
  assert_eq!(
    os_matches("win32"),
    cfg!(target_os = "windows"),
    "a `win32` pattern is native only on Windows"
  );
  assert_eq!(
    os_matches("linux"),
    cfg!(target_os = "linux"),
    "a `linux` pattern is native only on Linux"
  );
  assert_eq!(
    os_matches("darwin"),
    cfg!(target_os = "macos"),
    "a `darwin` pattern is native only on macOS"
  );
  assert!(
    !os_matches(""),
    "an empty tag is never `os_matches`: it is native everywhere via a different branch"
  );
}

/// An entry that declares no executables cannot be corroborated, so the
/// store id has to stand on its own: this is the case the tier exists for.
#[test]
fn argv_names_an_executable_passes_an_entry_with_no_executables() {
  let entry = ScannedEntry {
    id: "1".into(),
    name: "No Exes".into(),
    executables: Box::new([]),
    steam_ids: Box::new(["777".into()]),
    epic_ids: Box::new([]),
    aliases: Box::new([]),
  };
  for path in ["/opt/whatever", "/usr/bin/steamwebhelper", "hl2.exe"] {
    assert!(
      argv_names_an_executable(path, &entry),
      "{path} should pass when nothing is declared"
    );
  }
}

/// The automaton tier's boundary rule: a declared executable name has
/// to END the probed string on a path-component boundary. The
/// automaton matches a substring, so without this any process whose
/// path merely *contains* a name reads as the game.
///
/// Compared against the *normalized* name — Windows separators folded
/// and the launcher marker (`>`) turned into the leading `/`, exactly
/// what `normalize_exe_pattern` does before the pattern is built.
/// Comparing the raw name instead would reject every launcher entry
/// the automaton just accepted.
#[test]
fn exe_names_path_demands_a_component_boundary() {
  for (name, path) in [
    // The relative names the database spells, as a Proton `argv[0]`
    // carries them: the name sits at the end.
    ("hl2.exe", "/C:/Steam/steamapps/common/GarrysMod/hl2.exe"),
    ("hl2.exe", "/C:/Steam/steamapps/common/garrysmod/HL2.EXE"),
    (
      "garrysmod/hl2.exe",
      "/C:/Steam/steamapps/common/GarrysMod/garrysmod/hl2.exe",
    ),
    (
      "win64/gmod.exe",
      "/C:/Steam/steamapps/common/GarrysMod/win64/gmod.exe",
    ),
    // A declared `bin\gmad.exe` is the `bin/gmad.exe` the path carries.
    (
      "bin\\gmad.exe",
      "/C:/Steam/steamapps/common/GarrysMod/bin/gmad.exe",
    ),
    // The launcher marker is not part of the file name: it is kept raw
    // on the entry for the argument check in `finish_direct_hit`.
    (">hl2.exe", "/C:/Steam/steamapps/common/GarrysMod/hl2.exe"),
    // A name that carries its own leading separator owns its boundary.
    ("/hl2.exe", "/C:/Steam/steamapps/common/GarrysMod/hl2.exe"),
    // The bare name alone, as the database spells it.
    ("hl2.exe", "hl2.exe"),
    // Multi-byte components on the way in do not disturb the tail.
    ("a.exe", "/games/ä/a.exe"),
    // GTA V Enhanced: a multi-component name corroborates a bare path
    // (Proton/Heroic short launch) via the basename fallback.
    (
      "grand theft auto v enhanced/gta5_enhanced.exe",
      "gta5_enhanced.exe",
    ),
    // The real Heroic launch: `S:\Heroic\GTAVEnhanced\GTA5_Enhanced.exe`
    // normalized. The declared directory is absent from the short path,
    // so the bare basename still has to corroborate it.
    (
      "grand theft auto v enhanced/gta5_enhanced.exe",
      "/S:/Heroic/GTAVEnhanced/GTA5_Enhanced.exe",
    ),
    // The declared `bin` directory is absent from the path, so the
    // bare-basename fallback accepts the short launch: the basename
    // corroborates it rather than refusing it.
    (
      "bin/gmad.exe",
      "/C:/Steam/steamapps/common/GarrysMod/hl2.exe/gmad.exe",
    ),
  ] {
    assert!(
      exe_names_path(path, name),
      "{name:?} should name the tail of {path:?}"
    );
  }

  for (name, path) in [
    // A near miss is not a match: the boundary is a `/`, not any byte.
    ("hl2.exe", "/C:/Steam/steamapps/common/GarrysMod/xhl2.exe"),
    (
      "hl2.exe",
      "/C:/Steam/steamapps/common/GarrysMod/hl2.exe.txt",
    ),
    (
      "hl2.exe",
      "/C:/Steam/steamapps/common/GarrysMod/hl2.exe.evil",
    ),
    // A declared directory present as a substring suppresses the
    // fallback: near-miss components like `xgarrysmod` stay refused.
    (
      "garrysmod/hl2.exe",
      "/C:/Steam/steamapps/common/GarrysMod/xgarrysmod/hl2.exe",
    ),
    ("garrysmod/hl2.exe", "/tmp/xgarrysmod/hl2.exe"),
    ("bin/gmad.exe", "/tmp/binary/gmad.exe"),
    // A folder that ends the path is not the executable.
    ("hl2.exe", "/C:/Steam/steamapps/common/GarrysMod/hl2.exe/"),
    // The un-stripped original: the scan probes the stripped variant,
    // and the check reads the string that was probed.
    ("hl2.exe", "/C:/Steam/steamapps/common/GarrysMod/hl2_64.exe"),
    (
      ">hl2.exe",
      "/C:/Steam/steamapps/common/GarrysMod/hl2_64.exe",
    ),
    // A different game entirely.
    (
      "hl2.exe",
      "/C:/Steam/steamapps/common/deadlock/game/bin/win64/deadlock.exe",
    ),
    // Shorter than the name.
    ("hl2.exe", "hl2.ex"),
    // A name that normalizes to a bare `/` is dropped at bundle build
    // time, because it would match every path: it names nothing.
    (">", "/anything"),
  ] {
    assert!(
      !exe_names_path(path, name),
      "{name:?} must not name the tail of {path:?}"
    );
  }
}

/// A macOS entry names the *bundle*, not the binary it runs.
///
/// 61 executables across 56 entries in the bundled database are spelled
/// `something.app` and all carry `os: "darwin"` — Factorio declares
/// `factorio.app` beside its Linux `factorio` and its `factorio.exe`.
/// But the process macOS actually runs is the binary inside the bundle,
/// `.../Factorio.app/Contents/MacOS/factorio`, so the declared name is a
/// component in the middle of the path and never its tail. Requiring the
/// tail is what refuses them.
///
/// The bundle is the unit the database names, and the interior is
/// Apple's, fixed for every bundle ever shipped: `Contents/MacOS/`. So a
/// declared `.app` name sitting at a component boundary with that
/// interior behind it does name the process — and nothing else does.
#[test]
fn a_declared_app_bundle_names_the_binary_it_runs() {
  for (name, path) in [
    // The real Factorio layout.
    (
      "factorio.app",
      "/Applications/Factorio.app/Contents/MacOS/factorio",
    ),
    // The bundle mounted from a Steam library rather than /Applications.
    (
      "factorio.app",
      "/Users/x/Library/Application Support/Steam/SteamApps/common/Factorio.app/Contents/MacOS/factorio",
    ),
    // Lowercase, and the interior spelled as the filesystem holds it.
    (
      "factorio.app",
      "/games/factorio.app/contents/macos/factorio",
    ),
    // A bundle whose executable does not repeat its own name.
    (
      "the binding of isaac rebirth.app",
      "/Applications/The Binding of Isaac Rebirth.app/Contents/MacOS/isaac",
    ),
  ] {
    assert!(
      exe_names_path(path, name),
      "{name:?} names the bundle this binary runs inside"
    );
  }

  for (name, path) in [
    // The interior is what makes it a bundle. Without it, a name in the
    // middle of a path is the substring the whole rule exists to refuse
    // — this is `hl2.exe.txt` wearing a different extension.
    ("factorio.app", "/games/factorio.app/something/factorio"),
    // A different bundle entirely.
    (
      "factorio.app",
      "/Applications/Subnautica.app/Contents/MacOS/subnautica",
    ),
    // The boundary still has to be a `/`.
    (
      "factorio.app",
      "/Applications/xfactorio.app/Contents/MacOS/factorio",
    ),
    // And the name still has to match whole: `factorio.app` is not a
    // prefix of `factorio.app2`.
    (
      "factorio.app",
      "/Applications/factorio.app2/Contents/MacOS/factorio",
    ),
    // A non-`.app` name is untouched by any of this: it sits in the
    // middle of the path, it is not the tail, and there is no bundle
    // rule to catch it.
    ("hl2.exe", "/Applications/x/hl2.exe/Contents/MacOS/launcher"),
  ] {
    assert!(
      !exe_names_path(path, name),
      "{name:?} must not name {path:?} — no bundle of that name runs it"
    );
  }
}

/// The two spellings `normalize_exe_pattern` folds before a pattern is
/// built — the launcher marker (arrpc's "the arguments carry the
/// launcher", kept raw on the entry for `finish_direct_hit`) and Windows
/// separators — have to name the process in the store-id tier too.
///
/// Comparing the raw spelling rejects both: `strip_suffix(">hl2.exe")`
/// and `strip_suffix("bin\gmad.exe")` can never match a `/`-separated
/// path. No entry in the bundled database is affected today — every one
/// that declares such a name also declares a plain one, and none uses a
/// backslash at all — so this is a latent divergence and not a live
/// false negative. Pinned anyway: it is the same rule the automaton
/// tier corroborates with, and two rules for one question is how the
/// other divergence on this branch survived a review.
#[test]
fn argv_names_an_executable_matches_what_the_automaton_normalizes() {
  use crate::types::OsName;
  use crate::types::ScannedExe;

  for (name, path) in [
    (">hl2.exe", "/C:/Steam/steamapps/common/GarrysMod/hl2.exe"),
    (
      "bin\\gmad.exe",
      "/C:/Steam/steamapps/common/GarrysMod/bin/gmad.exe",
    ),
  ] {
    // One executable, spelled the way the database spells it: the entry
    // has no plain name to fall back on, so the spelling under test is
    // the only evidence.
    let entry = ScannedEntry {
      id: "1".into(),
      name: "Garry's Mod".into(),
      executables: Box::new([ScannedExe {
        name: name.into(),
        os: OsName::Win32,
        is_launcher: false,
        arguments: None,
      }]),
      steam_ids: Box::new(["4000".into()]),
      epic_ids: Box::new([]),
      aliases: Box::new([]),
    };
    assert!(
      argv_names_an_executable(path, &entry),
      "{name:?} should name {path} exactly as the automaton's pattern does"
    );
  }
}
