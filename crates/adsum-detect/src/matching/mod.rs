//! Classification: one process in, an optional hit out.
//!
//! Cheapest source first (native AC, cwd-joined bare exe, authoritative
//! Steam AppId, Epic sandbox id or its `-epicapp` fallback, Proton AC,
//! install-dir tail, stem/folder heuristics).
//! Lazy per-pid reads happen only on misses/hits — never for the table.
//!
//! # Layout
//!
//! This module holds the pipeline itself (`match_process`, the automaton
//! probes, the outcome walk) plus the sidecar-name gate; its four
//! submodules hold the rule families it funnels through:
//! `normalize` (map-key/lookup name folding), `paths` (what it means
//! for a declared name to name a path), `args` (command-line store
//! tokens) and `tiers` (the aux match tiers and the shared hit tail).
//! The result post-processing (`first_sightings`, `apply_ignore_list`)
//! lives in [`runtime`](crate::runtime) with the lanes that consume it,
//! and `os_matches` moved beside [`OsName`](crate::types::OsName) in
//! [`types`](crate::types).

use std::sync::Arc;

use crate::bundle::{DetectablesBundle, bare_exe, path_variants_into};
use crate::db::Exclusions;
use crate::platform::read_cwd_into;
use crate::scratch::{ProbeBufs, ScanScratch};
use crate::server::ProcessServer;
use crate::types::{DetectSource, Exec, ScannedEntry, ScannedHit};
use adsum_types::AppId;

mod args;
mod normalize;
mod paths;
mod tiers;

// The historical public surface (these paths were `scan::` before the
// split): embedders and the in-crate suites import them from here.
pub use args::{app_id_from_args, epic_app_from_args, epic_id_from_args};
pub use normalize::{
  name_matchable, normalize_lowered_dedotted_into, normalize_lowered_into, normalize_name,
  normalize_name_into,
};
pub use paths::exe_stem;
pub use tiers::{SteamIdLookup, match_epic_id, match_name_or_folder, match_steam_id};

pub(crate) use args::{epic_launch_args, is_shortcut_id};
pub(crate) use paths::exe_names_path;
#[cfg(test)]
pub(crate) use tiers::argv_names_an_executable;
pub(crate) use tiers::finish_direct_hit;

// Test-only surfaces: the in-crate suites glob this module (`use
// super::*`), so everything they drove on the old `scan` module stays
// reachable from here — the lazy readers (which live in `crate::platform`
// now), the OS-tag rule (which lives in `crate::types` now) and the
// corroboration seam.
#[cfg(test)]
pub(crate) use crate::platform::is_suspended;
#[cfg(all(target_os = "linux", test))]
pub(crate) use crate::platform::{read_exec, read_exec_into, read_steam_app_id};
#[cfg(test)]
pub(crate) use crate::types::os_matches;
#[cfg(test)]
pub(crate) use args::EpicArgs;

/// Basenames Steam launches on a game's behalf, none of which is the game
/// itself.
///
/// Steam ends an update by pre-compiling shaders (`steamapprun` driving
/// `fossilize`/`fossilize-replay`) and by creating or upgrading the
/// Proton prefix (`wineserver`, `wineboot`), and it runs the game inside
/// the Steam Linux Runtime sandbox (`pressure-vessel` below `bwrap`,
/// `run-in-soldier`). It hands every one of those the game's
/// environment, `SteamAppId` included — so the store id that reaches the
/// scanner says Steam is working *on* the game, not that it is being
/// played. That is how updating a game opened a presence for it.
///
/// This is the cheap first line, not the whole rule. It keeps these names
/// out of every tier, including the ones the id corroboration cannot
/// judge — the library-prefix and folder tiers have no store id to check a
/// path against — and it costs one hash lookup. The set is deliberately
/// small, because a name it misses is caught by the corroboration inside
/// [`match_steam_id`], which is the rule that does not depend on a name
/// being known.
///
/// Evidence, not a guess: the launcher and helper basenames are the
/// binaries a Steam install actually ships — `Proton <ver>/proton` and
/// `Proton <ver>/files/bin/{wine,wineserver}`,
/// `SteamLinuxRuntime_*/{pressure-vessel,run,run-in-soldier}` — and the
/// pre-cache pair is named by Valve's own logs (`Fossilize`,
/// `steamapprun_pipeline_cache`, `fossilize_replay`). `bwrap` is the
/// primitive below pressure-vessel, and `steamwebhelper` is Steam's own
/// browser process; a game runs as a child of them, never as them.
pub const STEAM_SIDECAR_BASENAMES: [&str; 13] = [
  "steamapprun",
  "fossilize",
  "wineserver",
  "wineboot",
  "proton",
  "protonfixes",
  "pressure-vessel",
  "pressure-vessel-arm64",
  "bwrap",
  "steamwebhelper",
  "steam-runtime",
  "pv-wrap",
  "run-in-soldier",
];

/// Whether a process basename is a Steam sidecar: one that runs on a
/// game's behalf and therefore inherits its `SteamAppId`.
///
/// Lowercase basename in, matching the case the exclusions are consulted
/// with. Families are matched with a separator rather than as a bare
/// prefix: `fossilized` or `steam-runtimex` is a game's name, not one of
/// these.
#[inline]
pub fn is_steam_sidecar(basename: &str) -> bool {
  STEAM_SIDECAR_BASENAMES.contains(&basename)
    || matches_sidecar_family(basename, "fossilize")
    || matches_sidecar_family(basename, "steam-runtime")
    || matches_sidecar_family(basename, "pv-wrap")
}

/// Whether `basename` is a versioned member of a sidecar family: the base
/// name itself, or `base-suffix` / `base_suffix`, which is how Valve
/// versions its tools (`fossilize-replay`, `steam-runtime-soldier`).
#[inline]
fn matches_sidecar_family(basename: &str, base: &str) -> bool {
  basename
    .strip_prefix(base)
    .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('_'))
}

/// What one automaton's walk over a variant concluded about the process.
///
/// The middle case is the one that matters: the probed string *carries*
/// declared executable names, but none of them names it — `.../Portal 2/
/// portal2.exe.txt` holds `portal2.exe` without ending with it. That is
/// specific evidence the process is not the game, and it is stronger than
/// anything the stem and folder tiers can say, so it is carried forward
/// instead of being flattened into an ordinary miss.
#[derive(Debug)]
enum MatchOutcome {
  /// A match that names the probed string on a component boundary.
  Corroborated(Arc<ScannedEntry>, usize),
  /// Declared names were seen in this string, and every one of them was
  /// refused: the process is running something that is *not* a declared
  /// executable of any entry.
  Refused,
  /// No declared name was found in this string at all.
  NoMatch,
}

impl MatchOutcome {
  /// The hit, when the walk corroborated one.
  fn take_hit(&self) -> Option<(Arc<ScannedEntry>, usize)> {
    match self {
      Self::Corroborated(obj, exe_index) => Some((obj.clone(), *exe_index)),
      Self::Refused | Self::NoMatch => None,
    }
  }

  /// Whether this outcome saw a declared name that did not name the
  /// process: evidence strong enough that the looser stem and folder
  /// tiers must not answer afterwards.
  fn refused(&self) -> bool {
    matches!(self, Self::Refused)
  }
}

/// First of `probe`'s matches that names the end of `variant` on a
/// component boundary, if any.
///
/// The automaton reports the earliest-*ending* match, which is not
/// necessarily the one that names the process: a shorter declared name
/// from another entry can end *before* it, inside the same path — `java`
/// inside `illarion/jre/bin/java.exe`. Refusing that match is correct, but
/// refusing the *variant* with it would lose the game the path does name,
/// so the walk continues through the variant's remaining matches in the
/// same order a single `find` used to return them. Each refusal is logged,
/// because a boundary rule that is too tight is a silent false negative.
///
/// Generic in the probe so the three automatons, whose iterators are
/// different types, share this one corroboration.
fn first_hit_naming_path(
  probe: impl Iterator<Item = (Arc<ScannedEntry>, usize)>,
  variant: &str,
) -> MatchOutcome {
  let mut any_match = false;
  for (obj, exe_index) in probe {
    if obj
      .executables
      .get(exe_index)
      .is_some_and(|exe| exe_names_path(variant, &exe.name))
    {
      return MatchOutcome::Corroborated(obj, exe_index);
    }
    any_match = true;
    tracing::debug!(
      "[Process Scanner] {} does not name a declared executable of {}, skipping",
      variant,
      obj.name
    );
  }
  if any_match {
    MatchOutcome::Refused
  } else {
    MatchOutcome::NoMatch
  }
}

impl ProcessServer {
  /// Main-DB half of [`ProcessServer::ac_probe`]: every path variant is
  /// probed here before any variant reaches custom overrides, so a main
  /// pattern always beats a custom one regardless of 64-bit stripping.
  ///
  /// Yields the matches in scan order (earliest-ending first — the same
  /// one `find` returned), *overlapping* included: a shorter declared
  /// name can sit inside the path (`java` inside
  /// `illarion/jre/bin/java.exe`) and end before the match that actually
  /// names the process, so the caller has to be able to refuse that one
  /// and keep looking at the rest of the variant instead of giving up on
  /// the variant as a whole. Overlapping iteration is what makes those
  /// two visible to the same walk.
  fn main_probe<'a>(
    &self,
    reversed_path: &'a str,
    bundle: &'a DetectablesBundle,
  ) -> impl Iterator<Item = (Arc<ScannedEntry>, usize)> + 'a {
    bundle
      .ac
      .find_overlapping_iter(reversed_path)
      .filter_map(move |mat| {
        let pair = bundle.indexes.get(mat.pattern().as_usize())?.resolve();
        Some((bundle.list.get(pair.0)?.clone(), pair.1))
      })
  }

  /// Custom-overrides half of [`ProcessServer::ac_probe`]. Same scan-order
  /// overlapping walk as [`main_probe`](ProcessServer::main_probe).
  fn custom_probe<'a>(
    &self,
    reversed_path: &'a str,
    bundle: &'a DetectablesBundle,
  ) -> impl Iterator<Item = (Arc<ScannedEntry>, usize)> + 'a {
    bundle.custom_ac.iter().flat_map(move |ac| {
      ac.find_overlapping_iter(reversed_path)
        .filter_map(move |mat| {
          let pair = bundle
            .custom_indexes
            .get(mat.pattern().as_usize())?
            .resolve();
          Some((bundle.custom.get(pair.0)?.clone(), pair.1))
        })
    })
  }

  /// Proton fallback probe (main DB `win32` entries on Linux): consulted
  /// only after the native patterns, user overrides and the authoritative
  /// Steam AppId all miss. Empty automaton off-Linux, so this is a cheap
  /// empty iterator there rather than a `None`.
  ///
  /// Same scan-order overlapping walk as the main-DB half: the callers
  /// refuse a match that does not name the probed string, and a refusal
  /// must cost that match, not the whole variant.
  ///
  /// # Breaking change
  ///
  /// Yields the matches instead of returning the first one. A caller that
  /// wants the old behaviour — the first match, corroborated by nobody —
  /// takes `.next()`, which is what the raw probe seam does. The scan loop
  /// needs every match, because the first one can be a different entry's
  /// shorter name that does not name the process, and stopping there
  /// loses the game that does.
  pub fn proton_probe<'a>(
    &self,
    reversed_path: &'a str,
    bundle: &'a DetectablesBundle,
  ) -> impl Iterator<Item = (Arc<ScannedEntry>, usize)> + 'a {
    bundle.proton_ac.iter().flat_map(move |ac| {
      ac.find_overlapping_iter(reversed_path)
        .filter_map(move |mat| {
          let pair = bundle
            .proton_indexes
            .get(mat.pattern().as_usize())?
            .resolve();
          Some((bundle.list.get(pair.0)?.clone(), pair.1))
        })
    })
  }

  /// Shared variant loop: try `path` plus its 64-bit-stripped variants
  /// against the native (`proton = false`) or Proton (`proton = true`)
  /// automaton. Precedence is per automaton, not per variant: every
  /// variant is probed against main first, then every variant against
  /// custom — so a main pattern always beats a custom one even when
  /// only a stripped variant collides (e.g. main `wow.exe` vs custom
  /// `wow64.exe`).
  ///
  /// A hit is only returned once it is corroborated: the automaton
  /// matches substrings, so the executable that fired has to name the end
  /// of the probed string on a component boundary
  /// ([`exe_names_path`](exe_names_path)). This is the one place that
  /// knows both halves at once — the string that was actually probed
  /// (the variant the 64-bit strip rewrote, or the cwd-joined candidate
  /// the caller built) and which executable matched it.
  fn probe_variants(
    &self,
    path: &str,
    probe: &mut ProbeBufs,
    bundle: &DetectablesBundle,
    proton: bool,
  ) -> MatchOutcome {
    let variant_count = path_variants_into(path, &mut probe.variants);
    // Automaton passes outer, variants inner (strict precedence). Without
    // user overrides there is no custom automaton: run a single pass
    // instead of probing a guaranteed-`None` second automaton per variant.
    let passes: usize = if proton || bundle.custom_ac.is_none() {
      1
    } else {
      2
    };
    // A refusal is a property of the *walk*, not of the probe that
    // happened to run last. Remembering it per probe let the next miss
    // erase it — the custom automaton returning nothing for a string the
    // main automaton had just refused — and a miss is precisely what lets
    // the folder tier answer.
    let mut refused = false;
    for pass in 0..passes {
      for variant in &probe.variants[..variant_count] {
        probe.reversed.clear();
        probe.reversed.extend(variant.chars().rev());
        // One automaton's matches for this variant, in scan order, until
        // one names the string that was probed. Generic in the probe so
        // the three automatons — which yield different iterator types —
        // share the corroboration below instead of repeating it.
        let outcome = if proton {
          first_hit_naming_path(self.proton_probe(&probe.reversed, bundle), variant)
        } else if pass == 0 {
          first_hit_naming_path(self.main_probe(&probe.reversed, bundle), variant)
        } else {
          first_hit_naming_path(self.custom_probe(&probe.reversed, bundle), variant)
        };
        refused |= outcome.refused();
        // A corroborated hit ends the walk. A refusal is remembered so the
        // looser tiers below can stand down, but the remaining variants
        // and the other automaton still get their turn.
        if let Some((obj, exe_index)) = outcome.take_hit() {
          return MatchOutcome::Corroborated(obj, exe_index);
        }
      }
    }
    if refused {
      MatchOutcome::Refused
    } else {
      MatchOutcome::NoMatch
    }
  }

  /// Classify one enumerated process, cheapest source first:
  /// native AC, bare-exe+cwd, authoritative Steam AppId, Epic sandbox id
  /// (or its `-epicapp` fallback), Proton (`win32`) AC, Steam install-dir,
  /// then the stem/folder heuristics — except when the launcher supplied a
  /// shortcut-range AppId (Steam-assigned non-Steam id): then stem/folder
  /// run before the install-dir. Lazy `/proc` reads (cwd, environ, stat)
  /// happen only on misses/hits respectively — never for the whole table.
  /// Extracted from the scan loop for reuse and testing; the loop itself
  /// just maps over it.
  #[hotpath::measure]
  pub fn match_process(
    &self,
    process: &Exec,
    bundle: &DetectablesBundle,
    excl: &Exclusions,
    obs_open: &mut bool,
    scratch: &mut ScanScratch,
  ) -> Option<ScannedHit> {
    // Process path with consistent slashes (original case: the
    // automata match ASCII case-insensitively). Borrowed until a
    // rewrite is actually needed — the common Linux case (no
    // backslashes, absolute path) allocates nothing at all.
    let mut process_path: std::borrow::Cow<str> = std::borrow::Cow::Borrowed(&process.path);

    if process_path.contains('\\') {
      process_path = std::borrow::Cow::Owned(process_path.replace('\\', "/"));
    }

    if !process_path.starts_with('/') {
      process_path = std::borrow::Cow::Owned(format!("/{process_path}"));
    }

    // Discord exclusions first: installers, crash reporters and friends
    // are invisible before any matching (one basename lookup instead of
    // the full probe chain, and they can never shadow a real game).
    // Before the OBS flag too: an excluded process is absent, period.
    // Only the tiny basename is lowercased (the exclusion list is) — and
    // only when it actually contains uppercase ASCII, so the common
    // already-lowercase path allocates nothing at all.
    let raw_basename = process_path.rsplit('/').next().unwrap_or(&process_path);
    let basename: std::borrow::Cow<str> = if raw_basename.bytes().any(|b| b.is_ascii_uppercase()) {
      std::borrow::Cow::Owned(raw_basename.to_ascii_lowercase())
    } else {
      std::borrow::Cow::Borrowed(raw_basename)
    };
    // The caller hoists the guard: one exclusions read per tick (or per
    // EXEC event), not one per process — the set changes hourly, and the
    // per-process lock acquire was two atomic RMWs on a shared cache
    // line for data that cannot move mid-tick.
    if excl.is_excluded(&basename) {
      tracing::debug!("[Process Scanner] Excluded process, skipping: {basename}");
      return None;
    }
    // Steam's own helpers, which inherit the game's `SteamAppId`. Before
    // any matching, like the exclusions above: a store id carried by a
    // launcher, a shader pre-cache or a prefix upgrade is evidence Steam
    // is working on the game, not that it is being played.
    //
    // A cheap first line, not a rule — the list never closes, and a new
    // Valve helper is a name nobody listed. It matches *process
    // basenames* and only those: `steamapprun_pipeline_cache`, which
    // Valve's own logs name while a game pre-caches shaders, is the
    // Fossilize serialization **path** (`.../fozpipelinesv6/
    // steamapprun_pipeline_cache.<hash>`), not a process — the process is
    // `fossilize`, which the family match already covers. Matching paths
    // would be a different rule than this one, and it is the boundary
    // check in the tiers below that actually rejects a helper.
    if is_steam_sidecar(&basename) {
      tracing::debug!("[Process Scanner] Steam helper process, skipping: {basename}");
      return None;
    }

    // OBS binaries ship lowercase; the flag only feeds an unconsumed
    // observer callback, so original-case matching is exact enough.
    if !*obs_open && (process_path.contains("obs64") || process_path.contains("streamlabs")) {
      *obs_open = true;
    }

    // Aho-Corasick matching against the path and its 64-bit-stripped
    // variants (so `wow64.exe` also matches a `wow.exe` pattern, like
    // arrpc/pog5-adsum). First hit in variant order wins.
    let mut found = self.probe_variants(&process_path, &mut scratch.probe, bundle, false);
    // Whether any automaton saw a declared name in this path that did not
    // name it. That is specific evidence the process is *not* the game,
    // carried down to the stem and folder tiers so they cannot hand a hit
    // back over it (see `MatchOutcome::Refused`).
    let mut refused = found.refused();
    // Which probe won (native path vs cwd-joined): tracked alongside
    // `found` because both funnel into one tail call below.
    let mut found_source = DetectSource::Automaton;

    // The reconstructed path, when the bare-exe probe built one: the
    // Proton probe below used to see only the bare `argv[0]` again, which
    // loses a `win32` entry whose declared name carries the directory the
    // cwd supplies (`bin/game.exe` for a bare `game.exe`). The store-id
    // tier keeps the raw path — a cwd-joined string is a guess, and the
    // authoritative tier should not corroborate on one.
    // The reconstructed path, when the bare-exe probe built one: the
    // Proton probe below used to see only the bare `argv[0]` again, which
    // loses a `win32` entry whose declared name carries the directory the
    // cwd supplies (`bin/game.exe` for a bare `game.exe`). The store-id
    // tier keeps the raw path — a cwd-joined string is a guess, and the
    // authoritative tier should not corroborate on one. The candidate
    // itself lives in the scratch's `cwd_join` reuse buffer; this flag
    // records that it was built (and stays valid: nothing refills that
    // buffer afterwards within the same process).
    let mut joined = false;

    // Proton bare-exe probe (DOOM Eternal case): argv[0] without
    // directories plus the process cwd often reconstructs the install
    // path the DB knows. Only for bare exes (paths with directories
    // already had their full match above); one readlink per miss, into
    // the caller's reuse buffers (zero allocations once warm).
    if found.take_hit().is_none()
      && let Some(exe) = bare_exe(&process_path)
      && read_cwd_into(
        process.pid,
        &mut scratch.match_scratch.cwd,
        &mut scratch.match_scratch.proc_path,
      )
      .is_some()
    {
      // Join into the reuse buffer: `read_cwd_into` guarantees the cwd
      // bytes are valid UTF-8, so this never allocates or copies twice.
      {
        let cwd = std::str::from_utf8(&scratch.match_scratch.cwd).unwrap_or("");
        let candidate = &mut scratch.match_scratch.cwd_join;
        candidate.clear();
        candidate.push_str(cwd);
        candidate.push('/');
        candidate.push_str(exe);
      }
      tracing::debug!(
        "[Process Scanner] Bare exe, probing cwd-joined path for pid {}",
        process.pid
      );
      found = self.probe_variants(
        &scratch.match_scratch.cwd_join,
        &mut scratch.probe,
        bundle,
        false,
      );
      refused |= found.refused();
      if found.take_hit().is_some() {
        found_source = DetectSource::CwdJoined;
        tracing::debug!("[Process Scanner] Cwd match for pid {}", process.pid);
      } else {
        joined = true;
      }
    }

    let (obj, exe_index) = match found.take_hit() {
      Some(found) => found,
      None => {
        // Lowercase copy for the case-sensitive tail below (store-id
        // maps, stem/folder heuristics). Paid only on misses — the hot
        // AC path above never allocates it. Reuses the caller's buffer:
        // char-wise ASCII fold, byte-identical to `to_ascii_lowercase`.
        scratch.match_scratch.lowered.clear();
        scratch
          .match_scratch
          .lowered
          .extend(process_path.chars().map(|c| c.to_ascii_lowercase()));
        let lowered = scratch.match_scratch.lowered.as_str();
        // The AppId comes memoized (one environ read per process
        // lifetime); cmdline fallback when environ is unreadable
        // (sandboxed Proton runtimes hide it from service contexts).
        let app_id = self.cached_app_id(process.pid, &mut scratch.exec_scratch);
        let (app_id, via_cmdline) = match app_id {
          Some(id) => (Some(id), false),
          // Owned copy: only paid when environ is unreadable (sandboxed
          // Proton), so the common paths stay borrow-only.
          None => (
            app_id_from_args(process.arguments.as_deref()).map(AppId::from),
            true,
          ),
        };
        if via_cmdline && let Some(id) = app_id.as_deref() {
          tracing::debug!(
            "[Process Scanner] AppId {} for pid {} from command line (environ unreadable)",
            id,
            process.pid
          );
        }
        // Independent install-dir evidence: the path sits under the AppId's
        // own `steamapps/common/<installdir>/`. Skipped without an AppId,
        // and the comparison stays under the lock, so nothing is cloned.
        let install_dir_corroborated = app_id.as_deref().is_some_and(|id| {
          self.steam_install_tail_matches_app_id_into(
            lowered,
            id,
            &mut scratch.match_scratch.steam_path,
          )
        });
        // Authoritative Steam AppId first: a store id beats every fuzzy
        // path heuristic below.
        if let Some(hit) = match_steam_id(
          process.pid,
          &mut scratch.match_scratch.stat,
          &SteamIdLookup {
            app_id: app_id.as_deref(),
            process_path: lowered,
            source: DetectSource::SteamAppId,
            steam_map: &bundle.steam_map,
            entries: &bundle.list,
            custom: &bundle.custom,
            install_dir_corroborated,
          },
        ) {
          return Some(hit);
        }
        // Epic launch tokens in one pass: the sandbox id is the first
        // choice; older Legendary (<0.20.25) passes only `-epicapp`, which
        // Heroic's cached library maps to the same namespace the database
        // stores. Usually already lowercase: borrow it, fold on demand.
        let epic = epic_launch_args(process.arguments.as_deref());
        let sandbox_id: Option<std::borrow::Cow<'_, str>> = epic.sandbox_id.map(|id| {
          if id.bytes().any(|b| b.is_ascii_uppercase()) {
            std::borrow::Cow::Owned(id.to_ascii_lowercase())
          } else {
            std::borrow::Cow::Borrowed(id)
          }
        });
        if let Some(id) = sandbox_id.as_deref()
          && let Some(hit) = match_epic_id(
            process.pid,
            &mut scratch.match_scratch.stat,
            id,
            lowered,
            &bundle.epic_map,
            &bundle.list,
            &bundle.custom,
          )
        {
          return Some(hit);
        }
        // App-name fallback: the read lock is held only across the lookup
        // and the match it feeds, never across the rest of the scan.
        if let Some(app_name) = epic.app_name {
          let heroic = self.heroic_apps.read().unwrap_or_else(|e| e.into_inner());
          if let Some(namespace) = heroic.namespace_for(app_name)
            && sandbox_id.as_deref() != Some(namespace)
            && let Some(hit) = match_epic_id(
              process.pid,
              &mut scratch.match_scratch.stat,
              namespace,
              lowered,
              &bundle.epic_map,
              &bundle.list,
              &bundle.custom,
            )
          {
            return Some(hit);
          }
        }
        // An AppId the launcher gave us but the database doesn't know needs
        // a second look: Steam itself assigns high-bit-set ids
        // (e.g. 2532755798) to non-Steam shortcuts, while real store ids
        // are small. A shortcut-range id is a self-identified non-Steam
        // game, so its own exe/folder name beats install location; a
        // small unknown id is a real game missing from the DB, where
        // location (Steam's ground truth) still beats name guessing.
        let non_steam = app_id.as_deref().is_some_and(is_shortcut_id);
        // A refusal already happened: an automaton saw a declared name in
        // this path that did not name it. The name rules are deliberately
        // looser, and answering after one would hand the hit back over the
        // specific evidence — `/opt/Portal 2/portal2.exe.txt` must not come
        // back as Portal 2 from its folder.
        //
        // It does not carry as far as the Proton probe. That automaton has
        // not run yet and holds the `win32` names, and one entry's short
        // native name is a standing reason for it to hold the right one:
        // `hoi4` is the interior of Hearts of Iron IV's own `hoi4.exe`, and
        // `factorio` and `>java` are the interiors of theirs. Returning
        // here would let a Linux name hide a Wine game the very next probe
        // was about to name.
        if refused {
          tracing::debug!(
            "[Process Scanner] {} refused a declared executable match, not answering from its name",
            lowered
          );
        } else if non_steam
          && let Some(hit) = match_name_or_folder(
            lowered,
            process.pid,
            &bundle.name_map,
            &bundle.name_map_nodot,
            &bundle.list,
            &mut scratch.match_scratch.norm_out,
            &mut scratch.match_scratch.stat,
          )
        {
          return Some(hit);
        }
        // Proton fallback: `win32` executables from the main DB. Catches
        // Wine/Proton games whose store id is unreadable and whose exe is
        // too generic for the stem/folder heuristics. The bare-exe probe
        // above may have reconstructed a fuller path from the process
        // cwd; that reconstruction is exactly what this probe needs, so
        // it is preferred over the bare `argv[0]` when it exists.
        // The cwd-joined reconstruction (when the bare-exe probe built
        // one) is what this probe needs: it recovers the directories the
        // bare `argv[0]` lost. Lives in the scratch reuse buffer; the
        // borrow is disjoint from the `lowered`/`norm_out`/`stat` field
        // borrows below (same contract the whole match path runs under).
        let proton_path: &str = if joined {
          &scratch.match_scratch.cwd_join
        } else {
          &process_path
        };
        let proton = self.probe_variants(proton_path, &mut scratch.probe, bundle, true);
        refused |= proton.refused();
        if let Some((obj, exe_index)) = proton.take_hit() {
          return finish_direct_hit(
            &obj,
            exe_index,
            process,
            DetectSource::ProtonAutomaton,
            &mut scratch.match_scratch.stat,
          );
        }
        // The Proton probe runs after the check above, so a refusal of its
        // own is only settled here — and there are still two looser tiers
        // below it.
        if refused {
          tracing::debug!(
            "[Process Scanner] {} refused a declared executable match, not guessing further",
            lowered
          );
          return None;
        }
        // Steam's own word: the process runs under a known install dir,
        // so it inherits that entry's AppId. Beats name guessing below,
        // loses to a DB-declared path above. Tail matching (not the host
        // prefix) is what reaches Proton paths like `S:/steamapps/...`;
        // looked up lazily here because the AppId tier above already
        // covered every process whose store id corroborates the path.
        if let Some(library_appid) =
          self.steam_install_tail_app_id_into(lowered, &mut scratch.match_scratch.steam_path)
          && let Some(hit) = match_steam_id(
            process.pid,
            &mut scratch.match_scratch.stat,
            &SteamIdLookup {
              app_id: Some(&library_appid),
              process_path: lowered,
              source: DetectSource::SteamLibrary,
              steam_map: &bundle.steam_map,
              entries: &bundle.list,
              custom: &bundle.custom,
              // The id came from this same path, so it cannot corroborate
              // itself: keep requiring a declared executable (or none).
              install_dir_corroborated: false,
            },
          )
        {
          tracing::debug!(
            "[Process Scanner] Steam library match: {} (appid {})",
            hit.entry.name,
            library_appid
          );
          return Some(hit);
        }
        // Last resort: exe-stem == game name, then install-folder walk
        // (already consulted above for shortcut-range AppIds).
        if non_steam {
          return None;
        }
        return match_name_or_folder(
          lowered,
          process.pid,
          &bundle.name_map,
          &bundle.name_map_nodot,
          &bundle.list,
          &mut scratch.match_scratch.norm_out,
          &mut scratch.match_scratch.stat,
        );
      }
    };

    finish_direct_hit(
      &obj,
      exe_index,
      process,
      found_source,
      &mut scratch.match_scratch.stat,
    )
  }

  /// Single reversed-path AC probe (main DB, then custom overrides).
  /// Shared lookup half of the scan: direct-path and cwd-joined probes
  /// match identically through here.
  ///
  /// Probe seam (also used by tests): production classifies through
  /// `probe_variants` (see below), which probes every path variant
  /// against the main database before any variant reaches the custom
  /// overrides.
  pub fn ac_probe(
    &self,
    reversed_path: &str,
    bundle: &DetectablesBundle,
  ) -> Option<(Arc<ScannedEntry>, usize)> {
    // Same-bundle automaton + indexes: the ids this find() returns can
    // only index the table they were built with. No locks, no tearing.
    // First match per automaton, main before custom: this seam is the raw
    // probe — the boundary corroboration lives in `probe_variants`.
    self
      .main_probe(reversed_path, bundle)
      .next()
      .or_else(|| self.custom_probe(reversed_path, bundle).next())
  }
}

#[cfg(test)]
#[path = "../../tests/unit/matching/mod.rs"]
mod tests;
