//! The auxiliary match tiers, in pipeline order: the authoritative
//! Steam AppId, the Epic sandbox id / `-epicapp` namespace, and the
//! heuristic exe-stem / install-folder walk — plus the shared hit tail
//! (argument check, suspended check, timestamp stamp) and the
//! corroboration rules that decide whether a store id may stand for the
//! process that carries it.
//!
//! These run only on automaton misses, so their lookups (sorted-index
//! binary searches, folder normalization) never touch the hot path.

use std::sync::Arc;

use crate::bundle::SortedIndex;
use crate::platform::is_suspended;
use crate::types::{DetectSource, Exec, ScannedEntry, ScannedHit};

use super::normalize::{name_matchable, normalize_lowered_dedotted_into, normalize_lowered_into};
use super::paths::{exe_names_path, exe_stem};

/// Drop matches on suspended processes (see [`is_suspended`]): a SIGSTOP'd
/// game shows a frozen frame at best — it is not being played. Single
/// choke point for every aux hit (appid/stem/folder), mirroring the scan
/// loop's post-match check for AC hits. Stamps the observation onto the
/// shared slim entry (no clone).
///
/// `stat` is the shared reuse buffer for the suspension read.
fn live_or_none(
  obj: &Arc<ScannedEntry>,
  pid: u64,
  source: DetectSource,
  stat: &mut String,
) -> Option<ScannedHit> {
  if is_suspended(pid, stat) {
    tracing::debug!("[Process Scanner] Ignoring suspended process (pid {pid})");
    return None;
  }
  Some(ScannedHit::stamp(obj.clone(), pid, source))
}

/// Shared tail of every direct path hit (native or Proton): when the
/// database declares `arguments` for an executable, the process command
/// line must contain them (parity with arrpc/pog5-adsum, e.g. TF2
/// `-game tf`); then the suspended check + timestamp stamp via
/// `live_or_none`.
///
/// `stat` is the shared reuse buffer for the suspension read.
pub(crate) fn finish_direct_hit(
  obj: &Arc<ScannedEntry>,
  exe_index: usize,
  process: &Exec,
  source: DetectSource,
  stat: &mut String,
) -> Option<ScannedHit> {
  // A hit without executables (or a stale index) is corrupt input, not a
  // game: skip the process instead of panicking the scan.
  let executable = obj.executables.get(exe_index)?;

  if let Some(exec_args) = &executable.arguments {
    let has_args = process
      .arguments
      .as_ref()
      .is_some_and(|args| args.contains(&**exec_args));
    if !has_args {
      tracing::debug!(
        "[Process Scanner] Argument mismatch for pid {}, skipping",
        process.pid
      );
      return None;
    }
  }

  live_or_none(obj, process.pid, source, stat)
}

/// Whether `process_path` names an executable the entry declares.
///
/// A store id says which game was *launched*, not which process is running:
/// Steam ends an update by running the app's script phase and setting up the
/// Proton prefix, and under Proton those run with the environ hidden from a
/// service context — so the id reaches the scanner by way of the command
/// line, exactly as it did while Garry's Mod was committing 3005 files. What
/// tells a helper from the game is the executable: a game process carries the
/// game's own binary as `argv[0]`, and a launcher never does.
///
/// Lowercased `/`-separated path in, matched as a path suffix, so a Windows
/// `argv[0]` under Proton (`/C:/…/steamapps/common/garrysmod/hl2.exe`) matches
/// the relative names the database declares (`hl2.exe`, `garrysmod/hl2.exe`,
/// `win64/gmod.exe`).
///
/// The comparison is the same one the automaton tier corroborates its hits
/// with ([`exe_names_path`]), deliberately: one question — does this name
/// name this process? — has one rule, including the launcher marker and the
/// Windows separators `normalize_exe_pattern` folds before a pattern is
/// built. Comparing the raw spelling instead would reject an entry that
/// declared only `>hl2.exe` or `bin\gmad.exe`. It also compares in place,
/// so the per-name lowercased copy this loop used to build is gone.
pub(crate) fn argv_names_an_executable(process_path: &str, obj: &ScannedEntry) -> bool {
  // Nothing declared that can name the game: the store id is the only
  // evidence there is, so it stands on its own. This is the case the id
  // tier exists for.
  //
  // It is also the tier's largest hole, and knowingly so: 35% of the
  // database declares no executables, so a helper that inherits such an
  // entry's id still reads as the game. The sidecar list trims the known
  // Valve helpers and the boundary rule covers the entries that *do*
  // declare executables, but nothing here can reject an unknown helper
  // for an entry that declares nothing. Dropping the fallback would
  // trade that for a silent false negative on every such game, which is
  // why the limit is stated instead of closed.
  if obj.executables.is_empty() {
    return true;
  }
  // A launcher is not the game, and the database flags them as such
  // (`is_launcher`). Steam launches the launcher with the game's store
  // id, so a process can carry Baldur's Gate III's id while running only
  // `larilauncher.exe` — the same inherited-id shape as the `reaper`. The
  // automaton never had a pattern for one of these (`collect_split_patterns`
  // skips them), so without this filter the two tiers disagree about
  // exactly the process the flag exists to describe. The `>` marker on a
  // name is a different thing: it is the arrpc "arguments carry the
  // launcher" spelling, and those entries are *not* flagged here.
  //
  // Every entry in the bundled database that declares a launcher also
  // declares the game's own executables, so the filter costs no detection
  // on current data. An entry that declared *only* launchers would lose
  // the id tier — none does, and it would have nothing that names the
  // game to be corroborated by.
  obj
    .executables
    .iter()
    .filter(|exe| !exe.is_launcher)
    .any(|exe| exe_names_path(process_path, &exe.name))
}

/// Whether `process_path` names an executable the entry flags as a launcher.
/// The install-dir evidence must never promote one to the game.
fn argv_names_a_launcher(process_path: &str, obj: &ScannedEntry) -> bool {
  obj
    .executables
    .iter()
    .filter(|exe| exe.is_launcher)
    .any(|exe| exe_names_path(process_path, &exe.name))
}

/// Prerequisite shelves Steam runs with the game's AppId but that are not
/// the game: checked below the install dir, with a file component after.
const INSTALLER_SHELVES: [&str; 4] = ["_commonredist", "_redist", "redist", "directx"];

/// Whether any component below `steamapps/common/<installdir>/` is a
/// prerequisite shelf; paths without that layout are unaffected.
fn names_an_installer_shelf(process_path: &str) -> bool {
  let mut components = process_path
    .split(['/', '\\'])
    .filter(|component| !component.is_empty())
    .peekable();
  while let Some(component) = components.next() {
    if component.eq_ignore_ascii_case("steamapps")
      && components
        .next()
        .is_some_and(|next| next.eq_ignore_ascii_case("common"))
    {
      components.next(); // the install dir itself
      // Any shelf below counts, but it needs a file component after it:
      // an executable named `directx` is not a shelf.
      while let Some(component) = components.next() {
        if components.peek().is_some()
          && INSTALLER_SHELVES
            .iter()
            .any(|known| component.eq_ignore_ascii_case(known))
        {
          return true;
        }
      }
      return false;
    }
  }
  false
}

/// Whether the tier's evidence names `obj`: a declared executable, or the
/// install dir when the path is not a declared launcher or a prerequisite
/// installer shelf (Steam runs those with the game's AppId).
fn steam_id_corroborates(process_path: &str, obj: &ScannedEntry, install_dir: bool) -> bool {
  argv_names_an_executable(process_path, obj)
    || (install_dir
      && !argv_names_a_launcher(process_path, obj)
      && !names_an_installer_shelf(process_path))
}

/// One store-id lookup: what is being asked, and the tables that answer it.
///
/// Positional arguments ran out at eight, and the parts split naturally
/// anyway — the id being resolved, the evidence it has to be corroborated
/// with, and the database side that resolves it. Grouping them also keeps
/// the two callers honest: they read as "look this id up, against these
/// tables, for this process".
pub struct SteamIdLookup<'a> {
  /// The id to resolve, when one was found at all.
  pub app_id: Option<&'a str>,
  /// `argv[0]` of the process being classified, `/`-separated.
  pub process_path: &'a str,
  /// Which matcher is asking, so the hit reports where it came from.
  pub source: DetectSource,
  /// Store id to entry index.
  pub steam_map: &'a SortedIndex,
  /// The entries the index points into.
  pub entries: &'a [Arc<ScannedEntry>],
  /// User overrides, searched linearly (user-sized, not DB-sized).
  pub custom: &'a [Arc<ScannedEntry>],
  /// The caller proved the path sits under this AppId's
  /// `steamapps/common/<installdir>/`; the id alone never sets it.
  pub install_dir_corroborated: bool,
}

/// Authoritative aux lookup: `SteamAppId` (store games with empty
/// `executables`, legit Steam). Runs before every fuzzy heuristic — a
/// store id beats path guessing. Custom overrides participate as a
/// linear fallback (they are user-sized, not DB-sized): a custom entry
/// carrying only a steam distributor id matches here, since the map
/// only indexes the main list. Canonical map hits always win.
///
/// A hit is corroborated by the process path: the entry has to declare the
/// executable the process is running, matched on a component boundary, or —
/// when the caller proved the path sits under this AppId's
/// `steamapps/common/<installdir>/` — the path must not name a declared
/// launcher or sit on a prerequisite-installer shelf. That proof must come
/// from independent id evidence: a path-derived AppId cannot corroborate
/// itself. The id says which game was launched, and Steam launches things
/// on a game's behalf with that game's id — its update script phase, the
/// Proton prefix setup, the shader pre-cache — so an id alone is evidence of
/// work on the game rather than of play. An entry declaring no executables
/// is the case this tier exists for and wins on the id alone: there is
/// nothing to corroborate against.
///
/// # Breaking change
///
/// Takes a [`SteamIdLookup`] instead of eight positional arguments. The
/// old signature had no field for the process path — which is exactly
/// what the corroboration is made of — so a compatible wrapper would
/// have to guess at the evidence or silently skip the check. The parts
/// are grouped because two callers now read as one sentence, and because
/// the next field will not spill into a ninth positional.
pub fn match_steam_id(
  pid: u64,
  stat: &mut String,
  lookup: &SteamIdLookup<'_>,
) -> Option<ScannedHit> {
  let appid = lookup.app_id?;
  if let Some(idx) = lookup.steam_map.get(appid) {
    let obj = lookup.entries.get(idx)?;
    if !steam_id_corroborates(lookup.process_path, obj, lookup.install_dir_corroborated) {
      // The id is the game's, but this process is not the game: Steam's
      // own script/prefix/pre-cache work carries it. Named in the log,
      // because a rejected hit is otherwise indistinguishable from a miss.
      tracing::debug!(
        "[Process Scanner] AppId {appid} does not belong to {argv0} ({name}), skipping",
        argv0 = lookup.process_path,
        name = obj.name,
      );
      return None;
    }
    tracing::debug!(
      "[Process Scanner] Steam match: {} (appid {})",
      obj.name,
      appid
    );
    return live_or_none(obj, pid, lookup.source, stat);
  }
  // Custom override with a bare steam SKU (no executables to index):
  // the provenance travels through — a library-inherited app id matched
  // here still reports the library path that supplied it.
  for obj in lookup.custom {
    let sku_match = obj.steam_ids.iter().any(|id| {
      let sid: &str = id;
      sid == appid
    });
    if sku_match {
      if !steam_id_corroborates(lookup.process_path, obj, lookup.install_dir_corroborated) {
        tracing::debug!(
          "[Process Scanner] AppId {appid} does not belong to {argv0} ({name}, custom), skipping",
          argv0 = lookup.process_path,
          name = obj.name,
        );
        continue;
      }
      tracing::debug!(
        "[Process Scanner] Steam match (custom): {} (appid {})",
        obj.name,
        appid
      );
      return live_or_none(obj, pid, lookup.source, stat);
    }
  }
  None
}

/// Resolve an Epic namespace (sandbox id or Heroic app-name mapping) to an
/// entry, corroborated by the process path; mirrors [`match_steam_id`] and
/// runs before the fuzzy heuristics.
pub fn match_epic_id(
  pid: u64,
  stat: &mut String,
  epic_id: &str,
  process_path: &str,
  epic_map: &SortedIndex,
  entries: &[Arc<ScannedEntry>],
  custom: &[Arc<ScannedEntry>],
) -> Option<ScannedHit> {
  if let Some(idx) = epic_map.get(epic_id) {
    let obj = entries.get(idx)?;
    if !argv_names_an_executable(process_path, obj) {
      // The id is the game's, but this process does not run an executable
      // it declares: named in the log, because a rejected hit is
      // otherwise indistinguishable from a miss.
      tracing::debug!(
        "[Process Scanner] Epic id {epic_id} does not belong to {path} ({name}), skipping",
        path = process_path,
        name = obj.name,
      );
      return None;
    }
    tracing::debug!("[Process Scanner] Epic match: {} (id {epic_id})", obj.name);
    return live_or_none(obj, pid, DetectSource::EpicAppId, stat);
  }
  // Custom override carrying a bare epic SKU: searched linearly
  // (user-sized, not DB-sized), same corroboration as the map path.
  for obj in custom {
    if obj.epic_ids.iter().any(|id| id.as_ref() == epic_id) {
      if !argv_names_an_executable(process_path, obj) {
        tracing::debug!(
          "[Process Scanner] Epic id {epic_id} does not belong to {path} ({name}, custom), skipping",
          path = process_path,
          name = obj.name,
        );
        continue;
      }
      tracing::debug!(
        "[Process Scanner] Epic match (custom): {} (id {epic_id})",
        obj.name
      );
      return live_or_none(obj, pid, DetectSource::EpicAppId, stat);
    }
  }
  None
}

/// Heuristic aux lookup: exact exe-stem == multi-word
/// game name, then the install-folder walk. Runs after the Proton AC probe
/// in the scan loop — a DB-declared path (even `win32`) beats guessing.
pub fn match_name_or_folder(
  process_path: &str,
  pid: u64,
  name_map: &SortedIndex,
  name_map_nodot: &SortedIndex,
  detectable_list: &[Arc<ScannedEntry>],
  norm_out: &mut String,
  stat: &mut String,
) -> Option<ScannedHit> {
  let stem = exe_stem(process_path);
  if name_matchable(stem)
    && let Some(idx) = name_map.get(stem)
    && let Some(obj) = detectable_list.get(idx)
  {
    tracing::debug!(
      "[Process Scanner] Name match: {} (exe stem `{}`)",
      obj.name,
      stem
    );
    return live_or_none(obj, pid, DetectSource::ExeStem, stat);
  }

  // Install-folder fallback (Hydra / non-Steam shortcuts / renamed exes):
  // the folder often carries the title when the exe doesn't, e.g.
  // `.../Meccha Chameleon/MECCHA CHAMELEON/Chameleon/Binaries/Win64/
  // PenguinHotel-Win64-Shipping.exe`. Nearest ancestor wins; the map only
  // holds multi-word names, so generic folders (`binaries`, `win64`) and
  // single-word ones (`chameleon`) can never hit. Dotted components are
  // versions/hidden dirs, never titles.
  for component in process_path.rsplit('/').skip(1) {
    if component.contains('.') {
      continue;
    }
    // Hot path input is already lowercase (`&lowered` at both call
    // sites): skip the lowercase stage, refill the shared buffer.
    normalize_lowered_into(component, norm_out);
    let folder = norm_out.as_str();
    if name_matchable(folder)
      && let Some(idx) = name_map.get(folder)
      && let Some(obj) = detectable_list.get(idx)
    {
      tracing::debug!(
        "[Process Scanner] Folder match: {} (folder `{}`)",
        obj.name,
        folder
      );
      return live_or_none(obj, pid, DetectSource::Folder, stat);
    }
  }

  // Last tier: dotted titles (`R.E.P.O.`, `Q.U.B.E.`, `Mr. Bomber`).
  // Version/hidden-dir components can never match (single-word or short
  // after de-dotting, still gated) — but a dotted title folder can, so
  // compare de-dotted against the de-dotted twin map. Strictly after the
  // exact pass above, so exact matches always win.
  for component in process_path.rsplit('/').skip(1) {
    if !component.contains('.') {
      continue;
    }
    // De-dot + normalize in one pass into the shared buffer (see above
    // for the borrowing contract: lookups must end before the next
    // refill).
    normalize_lowered_dedotted_into(component, norm_out);
    let folder = norm_out.as_str();
    if name_matchable(folder)
      && let Some(idx) = name_map_nodot.get(folder)
      && let Some(obj) = detectable_list.get(idx)
    {
      tracing::debug!(
        "[Process Scanner] Folder match (de-dotted): {} (folder `{}`)",
        obj.name,
        folder
      );
      return live_or_none(obj, pid, DetectSource::Folder, stat);
    }
  }

  None
}
