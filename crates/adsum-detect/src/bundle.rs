//! One database generation: automata, index tables and entry lists.
//!
//! The atomic unit is the whole [`DetectablesBundle`], swapped via
//! `ArcSwap`: readers hold one `Arc` per tick and always observe a
//! self-consistent snapshot, never a torn mix mid-refresh.

use std::sync::Arc;

use aho_corasick::AhoCorasick;

use crate::db::DetectableActivity;
use crate::scan::{name_matchable, normalize_name, os_matches};
use crate::types::{OsName, ScannedEntry};

/// 64-bit markers a process path may carry (parity with arrpc/pog5-adsum).
///
/// A path is matched against each in turn: `/usr/lib64/x64/game64.exe`
/// strips down to `/usr/lib/x/game.exe` via `64`, and to
/// `/usr/lib64//game64.exe` via `x64` — which is what lets a
/// `/games/wow64.exe` process match a plain `wow.exe` database entry.
const MARKERS: [&str; 4] = ["64", ".x64", "x64", "_64"];

/// Sorted key→index table replacing a `HashMap` for the aux lookups: no
/// buckets (~33B each in SwissTable), no hashing, no per-key `String`
/// capacity field. Lookups happen only on match misses (rare per tick),
/// where O(log n) binary search over ~20k short keys is noise next to a
/// single `/proc` read. First-inserted index wins ties — identical to the
/// `or_insert` it replaces (callers insert canonical entries before
/// aliases, preserving canonical-first ties).
#[derive(Clone, Debug, Default)]
pub struct SortedIndex {
  entries: Vec<(Box<str>, usize)>,
}

impl SortedIndex {
  /// Build from `(key, index)` pairs: stable-sorts by key and keeps the
  /// first index per duplicate key (canonical-first ties, like the
  /// `or_insert` this replaces), then shrinks to fit.
  pub(crate) fn build(mut entries: Vec<(Box<str>, usize)>) -> Self {
    // Stable sort: equal keys keep insertion order, so `dedup_by` below
    // (which keeps the FIRST of each run) preserves first-wins.
    entries.sort_by(|a, b| {
      let (ka, kb): (&str, &str) = (&a.0, &b.0);
      ka.cmp(kb)
    });
    entries.dedup_by(|curr, prev| curr.0 == prev.0);
    entries.shrink_to_fit();
    Self { entries }
  }

  /// Look up one key by binary search: O(log n), only used on match
  /// misses (rare per tick).
  #[inline]
  pub fn get(&self, key: &str) -> Option<&usize> {
    self
      .entries
      .binary_search_by(|(k, _)| {
        let ka: &str = k;
        ka.cmp(key)
      })
      .ok()
      .map(|idx| &self.entries[idx].1)
  }

  /// Number of indexed keys (boot diagnostics).
  pub(crate) fn len(&self) -> usize {
    self.entries.len()
  }

  /// Membership probe (test-only; production only needs [`get`](SortedIndex::get)).
  pub fn contains_key(&self, key: &str) -> bool {
    self.get(key).is_some()
  }

  /// Iterate all entries (used to derive the de-dotted twin table).
  pub(crate) fn iter(&self) -> impl Iterator<Item = &(Box<str>, usize)> + '_ {
    self.entries.iter()
  }
}

/// Everything derived from one database generation, swapped atomically.
///
/// The atomic unit is automata, index tables, lists and aux maps. The
/// independent exclusions filter and Steam roots refresh alongside (two
/// swaps, not one): a tick may briefly mix generations. Harmless by
/// construction — exclusions only subtract, roots only add candidates —
/// so folding them into the bundle would buy nothing.
///
/// Rationale: the scanner used to hold the automaton, the index table,
/// the activity list and the aux maps under SEPARATE locks, replaced one
/// at a time by the hourly refresh. A scan landing mid-swap then probed a
/// Index pair into the generation: `[activity index, executable index]`.
///
/// Newtype, not a bare `[u32; 2]` alias, so the widening back to `usize`
/// lives in one place ([`resolve`](PatternIndex::resolve)) rather than
/// being repeated at every lookup — a type alias could not carry that
/// method.
///
/// `u32` halves: the pair is a dense table over tens of thousands of
/// entries, so 8 B instead of 16 B halves the table. The database is
/// bounded far below `u32::MAX` (`refresh` caps the body at 64 MiB and
/// the entry count is bounded), so a build that overflowed would already
/// have failed earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PatternIndex([u32; 2]);

impl PatternIndex {
  /// The pair as `usize` indices, ready to index a slice with.
  #[inline]
  pub(crate) fn resolve(self) -> (usize, usize) {
    (self.0[0] as usize, self.0[1] as usize)
  }
}

/// Convert a (activity, executable) pair to the compact index form.
///
/// Truncating an index would mis-attribute a game or panic the scan
/// thread, so it saturates and asserts instead: the database is bounded
/// far below `u32::MAX` (body capped at 64 MiB, entry count bounded above
/// ~50k), which makes this a build-time invariant, not a runtime path.
/// A correct build therefore never asserts.
fn pattern_index(activity_index: usize, exe_index: usize) -> PatternIndex {
  let activity = u32::try_from(activity_index).unwrap_or(u32::MAX);
  let exe = u32::try_from(exe_index).unwrap_or(u32::MAX);
  debug_assert!(
    activity < u32::MAX && exe < u32::MAX,
    "database too large for u32 index pairs: activity {activity_index}, exe {exe_index}"
  );
  PatternIndex([activity, exe])
}

/// NEW automaton against OLD indexes: index-out-of-bounds panic (dead
/// scan thread) or, worse, a silently mis-attributed game. With the whole
/// generation behind ONE Arc, readers clone it once per tick (or per
/// event) and can only ever observe a self-consistent snapshot; the
/// writer builds the next generation off-thread and swaps the pointer.
pub struct DetectablesBundle {
  /// Refcounted, not `Vec`: a rebuild clones this once (an Arc bump) instead
  /// of bumping every entry's refcount individually — ~24k of them on each
  /// database refresh or override append.
  pub(crate) list: Arc<[Arc<ScannedEntry>]>,
  pub(crate) ac: AhoCorasick,
  pub(crate) indexes: Vec<PatternIndex>,
  /// Proton fallback automaton (Linux only): `win32` executables from the
  /// main DB, probed after the native patterns and the authoritative Steam
  /// AppId, but before the stem/folder heuristics. Empty on other platforms.
  pub(crate) proton_ac: Option<AhoCorasick>,
  pub(crate) proton_indexes: Vec<PatternIndex>,
  /// Steam AppId (steam-distributor SKUs only) -> activity index.
  /// Lets us detect store games whose DB entry ships empty `executables`.
  /// Sorted vec, not a map: lookups only happen on match misses.
  pub(crate) steam_map: SortedIndex,
  /// Lowercased Epic sandbox id -> activity index, for Heroic/Epic launches
  /// whose environ the service sandbox hides.
  pub(crate) epic_map: SortedIndex,
  /// Normalized game name -> activity index, for the conservative exe-stem
  /// fallback (exact, multi-word names only, e.g. `how to fish`).
  /// Sorted vec, not a map: lookups only happen on match misses.
  pub(crate) name_map: SortedIndex,
  /// De-dotted twin of `name_map` above (only keys containing dots):
  /// last-tier fallback for dotted title folders (`R.E.P.O.`,
  /// `Q.U.B.E.`), which the exact walk skips as versions. Built by
  /// [`undotted_names`], same canonical-first ties.
  pub(crate) name_map_nodot: SortedIndex,
  pub(crate) custom: Arc<[Arc<ScannedEntry>]>,
  pub(crate) custom_ac: Option<AhoCorasick>,
  pub(crate) custom_indexes: Vec<PatternIndex>,
}

impl DetectablesBundle {
  /// Blind-but-valid generation for catastrophic automaton build
  /// failures (network-supplied DB exceeds builder limits): detection
  /// misses everything instead of panicking the daemon.
  fn empty() -> Self {
    Self {
      list: Arc::from(Vec::new()),
      ac: build_ac_automaton(&[]).expect("[bug] empty automaton always builds"),
      indexes: Vec::new(),
      proton_ac: None,
      proton_indexes: Vec::new(),
      steam_map: SortedIndex::default(),
      epic_map: SortedIndex::default(),
      name_map: SortedIndex::default(),
      name_map_nodot: SortedIndex::default(),
      custom: Arc::from(Vec::new()),
      custom_ac: None,
      custom_indexes: Vec::new(),
    }
  }
}

/// Exact upper bound for the name map: one slot per entry, plus one per
/// alias.
///
/// The alternative was to reserve twice the entry count, which at ~24k
/// entries buys over a megabyte of scratch for a table that holds a
/// fraction of it. The vector lives only for the build and is dropped
/// right after, so the slack is pure transient peak — paid at every
/// database refresh and override append.
pub fn name_map_bound(detectables: &[Arc<ScannedEntry>]) -> usize {
  detectables.len()
    + detectables
      .iter()
      .map(|entry| entry.aliases.len())
      .sum::<usize>()
}

/// Auxiliary lookup maps: Steam/Epic store id -> activity index, and
/// normalized game name -> activity index. Aliases join the name map
/// under the same conservative gate; canonical names win ties.
pub fn build_aux_maps(
  detectables: &[Arc<ScannedEntry>],
) -> (SortedIndex, SortedIndex, SortedIndex) {
  // Steam ids ≤ entries in practice.
  let mut steam: Vec<(Box<str>, usize)> = Vec::with_capacity(detectables.len());
  for (index, activity) in detectables.iter().enumerate() {
    for id in &activity.steam_ids {
      if !id.is_empty() {
        steam.push((id.clone(), index));
      }
    }
  }

  // Epic sandbox ids: ~2.5k dwell in the real database (~1 entry in 8),
  // so `len / 8` covers the table without over-buying build scratch.
  let mut epic: Vec<(Box<str>, usize)> = Vec::with_capacity(detectables.len() / 8);
  for (index, activity) in detectables.iter().enumerate() {
    for id in &activity.epic_ids {
      if !id.is_empty() {
        epic.push((id.clone(), index));
      }
    }
  }

  // Names reserve and fill in one place, so the bound cannot drift from
  // what the table can actually hold (see `collect_names`).
  let (steam_map, epic_map, name_map) = (
    SortedIndex::build(steam),
    SortedIndex::build(epic),
    SortedIndex::build(collect_names(detectables)),
  );
  tracing::info!(
    "[Process Scanner] Aux maps: {} steam ids, {} epic ids, {} matchable names",
    steam_map.len(),
    epic_map.len(),
    name_map.len()
  );
  (steam_map, epic_map, name_map)
}

/// Collect the name map's entries, reserving exactly what they can occupy.
///
/// Only matchable names participate in the exe-stem fallback, so generic
/// stems (`fish`, `steam`, `game`) can never collide with
/// `Fish`/`Steam`. Canonical names are inserted first and win ties.
///
/// The reservation is the exact bound rather than a multiple of the entry
/// count: `len * 2` over-bought by the alias deficit — at ~24k entries
/// that is most of a megabyte of scratch held only for the build. It is
/// also why the bound and the fill live together here: split apart, the
/// reserve can drift back to a guess nobody tests.
fn collect_names(detectables: &[Arc<ScannedEntry>]) -> Vec<(Box<str>, usize)> {
  let mut names: Vec<(Box<str>, usize)> = Vec::with_capacity(name_map_bound(detectables));
  for (index, activity) in detectables.iter().enumerate() {
    let normalized = normalize_name(&activity.name);
    if name_matchable(&normalized) {
      names.push((normalized.into_boxed_str(), index));
    }
    for alias in &activity.aliases {
      let normalized = normalize_name(alias);
      if name_matchable(&normalized) {
        names.push((normalized.into_boxed_str(), index));
      }
    }
  }
  names
}

/// De-dotted form for the folder-walk fallback tier: dots become
/// spaces (runs collapsed), so `R.E.P.O. Ghost Haul` compares equal on
/// the map-key side and the on-disk folder side alike.
pub(crate) fn dedot(name: &str) -> String {
  name
    .replace('.', " ")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

/// De-dotted twin of the name map, for the folder-walk fallback (see
/// [`match_name_or_folder`](crate::scan::match_name_or_folder)): only keys that actually contain dots, so
/// the extra table stays tiny. Built in canonical order (lowest index
/// wins ties), exactly like the main map.
pub fn undotted_names(name_map: &SortedIndex) -> SortedIndex {
  let mut dotted: Vec<(usize, Box<str>)> = name_map
    .iter()
    .filter(|(key, _)| key.contains('.'))
    .map(|(key, index)| (*index, key.clone()))
    .collect();
  // Lowest index first: `SortedIndex::build` keeps the first of each
  // duplicate key, so canonical entries win ties exactly like before.
  dotted.sort_by_key(|(index, _)| *index);
  SortedIndex::build(
    dotted
      .into_iter()
      .map(|(index, key)| (dedot(&key).into_boxed_str(), index))
      .collect(),
  )
}

/// Generate matching variants of a process path, removing 64-bit markers
/// (parity with arrpc/pog5-adsum). E.g. `/games/wow64.exe` produces
/// `/games/wow.exe` which matches a `wow.exe` database entry.
///
/// Writes into caller-owned buffers and returns how many are filled, so the
/// per-process scan allocates nothing at steady state: buffers are reused
/// across processes and scans, and the marker strip writes into the slot
/// it will occupy (see `remove_marker_into` below).
pub fn path_variants_into(path: &str, out: &mut [String; 5]) -> usize {
  out[0].clear();
  out[0].push_str(path);
  let mut count = 1;
  for marker in MARKERS {
    // Bound first: past the last slot there is nothing to write, and a
    // strip here would be work whose result is dropped.
    if !path.contains(marker) || count >= out.len() {
      continue;
    }
    // Write into the slot it would occupy, then decide: the candidate and
    // the already-written variants are disjoint ranges of `out`, so both
    // can be borrowed at once for the dedup check.
    let (filled, candidate) = out.split_at_mut(count);
    let candidate = &mut candidate[0];
    remove_marker_into(path, marker, candidate);
    if *candidate != path && !filled.iter().any(|variant| variant == candidate) {
      count += 1;
    }
  }
  count
}

/// Remove **every** occurrence of `marker`, writing into `buf` (cleared
/// first). Equivalent to `str::replace`, without its allocation.
///
/// This is what keeps `path_variants_into` allocation-free on the scan hot
/// path: the per-process walk hits a marker on most 64-bit Linux
/// processes — `/usr/lib64/...` alone matches — and a temp `String` per hit
/// was an allocation per process per tick.
pub(crate) fn remove_marker_into(path: &str, marker: &str, buf: &mut String) {
  buf.clear();
  let mut rest = path;
  while let Some(at) = rest.find(marker) {
    buf.push_str(&rest[..at]);
    rest = &rest[at + marker.len()..];
  }
  buf.push_str(rest);
}

/// Exe file name when the normalized `argv[0]` carries no directories
/// (bare exe, e.g. some Proton launches): `None` when directories are
/// present (the direct-path match covers those) or the name is empty.
#[inline]
pub fn bare_exe(normalized_path: &str) -> Option<&str> {
  let trimmed = normalized_path.strip_prefix('/').unwrap_or(normalized_path);
  if trimmed.is_empty() || trimmed.contains('/') {
    return None;
  }
  Some(trimmed)
}

/// Native + Proton pattern vectors from ONE walk over the entries (see
/// `collect_split_patterns`): same contents, same order, same filters as
/// the two separate walks this replaces — one normalization per exe.
struct SplitPatterns {
  native_patterns: Vec<String>,
  native_idx: Vec<PatternIndex>,
  proton_patterns: Vec<String>,
  proton_idx: Vec<PatternIndex>,
}

/// Walk the entries once, partitioning executables into the native set
/// (non-launcher, empty-OS or `os_matches`) and the Proton set (Linux
/// only: non-launcher `win32`). Eligibility mirrors
/// `build_ac_patterns_with_os_filter(enforce_os=true)` and the old
/// `build_proton_ac_patterns` exactly; the two sets are disjoint on
/// every platform, so each exe normalizes at most once — the same total
/// work as two walks, half the passes. Vectors start empty and grow with
/// eligible pushes only (launcher/OS/`/` filters reject most exes up
/// front); `build_bundle` shrinks the index vectors before they outlive
/// the build.
fn collect_split_patterns(detectables: &[Arc<ScannedEntry>]) -> SplitPatterns {
  // Proton collection is unneeded off Linux: leave it unallocated.
  let want_proton = cfg!(target_os = "linux");
  let mut split = SplitPatterns {
    native_patterns: Vec::new(),
    native_idx: Vec::new(),
    proton_patterns: Vec::new(),
    proton_idx: Vec::new(),
  };
  for (activity_index, activity) in detectables.iter().enumerate() {
    for (exe_index, executable) in activity.executables.iter().enumerate() {
      if executable.is_launcher {
        continue;
      }
      // Native eligibility: empty OS or this platform's tag.
      let native = executable.os.is_empty() || os_matches(executable.os.as_str());
      // Proton eligibility (Linux only): win32 exes. Disjoint from
      // native on Linux (`win32` never `os_matches` there); unreachable
      // elsewhere (`want_proton` is false).
      let proton = want_proton && executable.os == OsName::Win32;
      if !native && !proton {
        continue;
      }
      // Names normalizing to `/` (empty, lone separators, bare `>`)
      // match every reversed path: drop them instead of detecting
      // unrelated processes.
      let pattern = normalize_exe_pattern(&executable.name);
      if pattern == "/" {
        continue;
      }
      if native {
        split.native_patterns.push(pattern.clone());
        split
          .native_idx
          .push(pattern_index(activity_index, exe_index));
      }
      if proton {
        split.proton_patterns.push(pattern);
        split
          .proton_idx
          .push(pattern_index(activity_index, exe_index));
      }
    }
  }
  split
}

/// Custom-override patterns with no OS filter (user entries match anywhere).
fn build_ac_patterns_allow_all_os(
  detectables: &[Arc<ScannedEntry>],
) -> Result<(AhoCorasick, Vec<PatternIndex>), aho_corasick::BuildError> {
  build_ac_patterns_with_os_filter(detectables, false)
}

/// Build one self-consistent detection generation from slim entries:
/// every automaton, index table, list and aux map derived from the same
/// inputs. The caller swaps the resulting bundle in with a single pointer
/// write — readers never observe a torn mix, no matter when the refresh
/// lands. Callers convert full [`DetectableActivity`] inputs once via
/// [`ScannedEntry::from_activity`] before calling.
pub(crate) fn build_bundle(
  detectable: Arc<[Arc<ScannedEntry>]>,
  custom: Arc<[Arc<ScannedEntry>]>,
) -> Result<DetectablesBundle, aho_corasick::BuildError> {
  // One walk feeds both automata (see `collect_split_patterns`); each
  // source vector is dropped right after its automaton compiles, so the
  // peak is one automaton + one vector + builder scratch — never two
  // pattern vectors beside a live automaton.
  let split = collect_split_patterns(&detectable);
  let ac = build_ac_automaton(&split.native_patterns)?;
  let mut idx = split.native_idx;
  drop(split.native_patterns);
  let (proton_ac, mut proton_idx) = if split.proton_patterns.is_empty() {
    (None, split.proton_idx)
  } else {
    tracing::info!(
      "[Process Scanner] Proton fallback: {} win32 patterns",
      split.proton_patterns.len()
    );
    let automaton = build_ac_automaton(&split.proton_patterns)?;
    drop(split.proton_patterns);
    (Some(automaton), split.proton_idx)
  };
  // Indexes outlive the build inside the bundle: release growth slack
  // (amortized doubling) so the stored tables are exactly `len`.
  idx.shrink_to_fit();
  proton_idx.shrink_to_fit();
  tracing::info!(
    "[Process Scanner] Automata heap: native {} bytes, proton {} bytes",
    ac.memory_usage(),
    proton_ac.as_ref().map(|ac| ac.memory_usage()).unwrap_or(0)
  );
  let (steam_map, epic_map, name_map) = build_aux_maps(&detectable);
  let name_map_nodot = undotted_names(&name_map);
  let (custom_ac, custom_idx) = if custom.is_empty() {
    (None, Vec::new())
  } else {
    let (ac, mut idx) = build_ac_patterns_allow_all_os(&custom)?;
    idx.shrink_to_fit();
    (Some(ac), idx)
  };
  Ok(DetectablesBundle {
    list: detectable,
    ac,
    indexes: idx,
    proton_ac,
    proton_indexes: proton_idx,
    steam_map,
    epic_map,
    name_map,
    name_map_nodot,
    custom,
    custom_ac,
    custom_indexes: custom_idx,
  })
}

/// Shared automaton builder: one pattern per non-launcher executable plus
/// its index back into the entry list (empty input yields an empty matcher).
fn build_ac_patterns_with_os_filter(
  detectables: &[Arc<ScannedEntry>],
  enforce_os: bool,
) -> Result<(AhoCorasick, Vec<PatternIndex>), aho_corasick::BuildError> {
  let mut exe_patterns: Vec<String> = Vec::new();
  let mut exe_indexes: Vec<PatternIndex> = Vec::new();

  for (activity_index, activity) in detectables.iter().enumerate() {
    for (exe_index, executable) in activity.executables.iter().enumerate() {
      if executable.is_launcher {
        continue;
      }

      // Only build patterns for executables that could run on this platform
      // For custom overrides (enforce_os=false) we skip the OS filter entirely
      // so that win32 executables can be detected on Linux via Proton/Wine
      // — this is the fix for NFS HP Remastered etc that only ships win32 entries.
      if enforce_os && !executable.os.is_empty() && !os_matches(executable.os.as_str()) {
        continue;
      }

      // Names normalizing to `/` (empty, lone separators, bare `>`)
      // match every reversed path: drop them instead of detecting
      // unrelated processes.
      let pattern = normalize_exe_pattern(&executable.name);
      if pattern == "/" {
        continue;
      }
      exe_patterns.push(pattern);
      exe_indexes.push(pattern_index(activity_index, exe_index));
    }
  }

  Ok((build_ac_automaton(&exe_patterns)?, exe_indexes))
}

/// Build the automaton with ASCII case-insensitive matching: process
/// paths are compared in their original case, so the scan loop never
/// allocates a lowercased copy per process (the single hottest
/// allocation in the profiler). Slashes/case in patterns are normalized
/// at build time (rare), never per scan (hot).
fn build_ac_automaton(exe_patterns: &[String]) -> Result<AhoCorasick, aho_corasick::BuildError> {
  AhoCorasick::builder()
    .ascii_case_insensitive(true)
    .build(exe_patterns)
}

/// Normalize one DB executable name into a reversed AC pattern:
/// consistent slashes, leading `/` (`>` becomes `/`, arrpc parity).
/// Case is left alone — the automaton matches insensitively.
fn normalize_exe_pattern(name: &str) -> String {
  // Make paths consistent, and fix some additional checks
  let mut exec_name = name.replace('\\', "/");

  // Checks adapted from arrpc, remain the '>' in DetectableActivity for later argument checks
  if exec_name.starts_with(">") {
    exec_name.replace_range(0..1, "/");
  } else if !exec_name.starts_with("/") {
    exec_name.insert(0, '/');
  }

  exec_name.chars().rev().collect::<String>()
}

/// Build the initial detection generation, folding `custom` overrides
/// in: one automaton construction per boot instead of build-then-rebuild.
pub(crate) fn initial_bundle(
  detectable: Vec<Arc<DetectableActivity>>,
  custom: Vec<DetectableActivity>,
) -> Arc<DetectablesBundle> {
  tracing::info!(
    "[Process Scanner] Building Aho-Corasick patterns for main detectable activities..."
  );
  // Convert both sides straight to the slim scanner form: no
  // intermediate `Vec<Arc>` of full entries (24k Arcs built just to be
  // re-walked and dropped).
  let slim = |list: Vec<Arc<DetectableActivity>>| {
    list
      .iter()
      .map(|entry| Arc::new(ScannedEntry::from_activity(entry)))
      .collect::<Vec<_>>()
  };
  let custom: Vec<Arc<ScannedEntry>> = custom
    .into_iter()
    .map(|entry| Arc::new(ScannedEntry::from_owned(entry)))
    .collect();
  // Boxed once, here: everything downstream (the stored bundle plus every
  // rebuild that carries the main list unchanged) clones a refcount
  // instead of walking ~24k entries.
  let detectable: Arc<[Arc<ScannedEntry>]> = slim(detectable).into();
  let custom: Arc<[Arc<ScannedEntry>]> = custom.into();
  let bundle = Arc::new(build_bundle(detectable, custom).unwrap_or_else(|e| {
    tracing::warn!(
      "[Process Scanner] Bundled database failed to build ({}), starting blind",
      e
    );
    DetectablesBundle::empty()
  }));
  tracing::info!("[Process Scanner] Done!");
  bundle
}

#[cfg(test)]
#[path = "../tests/unit/bundle/mod.rs"]
mod tests;
