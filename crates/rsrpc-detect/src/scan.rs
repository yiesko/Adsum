//! Classification: one process in, an optional hit out.
//!
//! Cheapest source first (native AC, cwd-joined bare exe, authoritative
//! Steam AppId, Proton AC, install-dir prefix, stem/folder heuristics).
//! Lazy `/proc` reads happen only on misses/hits — never for the table.

use std::collections::HashSet;
use std::sync::Arc;

use crate::bundle::{DetectablesBundle, SortedIndex, bare_exe, path_variants_into};
use crate::server::ProcessServer;
use crate::types::{DetectSource, Exec, ScannedEntry, ScannedHit};
use rsrpc_types::AppId;

/// Read one process's cmdline into an `Exec` (Linux). `None` for kernel
/// threads, zombies, vanished or unreadable pids — the caller just skips
/// them; the periodic scan is the backstop.
///
/// Bounded: at most 64 KiB are read (`argv[0]` lives at the start, so
/// truncation only ever cuts late arguments, never the path). Adversarial
/// megabyte-cmdlines would otherwise multiply per process per tick.
#[cfg(target_os = "linux")]
pub fn read_exec(pid: u64) -> Option<Exec> {
  let mut exec = Exec::default();
  let mut scratch = ExecScratch::default();
  read_exec_into(pid, &mut exec, &mut scratch).then_some(exec)
}

/// Reusable intermediates for [`read_exec_into`]: one set per thread,
/// owned by the calling loop (scan thread, EXEC dispatch). The path
/// buffer, cmdline read buffer and args join buffer are allocated once
/// and cleared per pid, replacing the per-process-per-tick temporaries
/// (previously ~400 allocations every 5s). Fields are only touched by
/// the Linux reader; the type itself is part of every target's API.
#[derive(Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct ExecScratch {
  path: String,
  cmdline: Vec<u8>,
  args: String,
  /// `/proc/<pid>/environ` body for the AppId lookup: the only
  /// multi-kilobyte read of the scan. Reused across pids so the buffer
  /// keeps its capacity instead of regrowing per newly-seen process.
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  environ: Vec<u8>,
  /// Consecutive ticks where the live process count sat well under the
  /// slot count, so a single transient spike does not pin the extra
  /// slots (and their string buffers) for the rest of the session.
  pub(crate) low_ticks: u8,
}

/// Both scratch sets the scan loop owns per thread, grouped so the match
/// entry point takes one parameter instead of two (and stays under the
/// clippy argument limit).
///
/// They stay separate types (different fields, different readers): this
/// grouping is a call-shape convenience, not a merge — [`ExecScratch`]
/// also serves the exec reader on its own.
#[derive(Default)]
pub struct ScanScratch {
  /// Match-path buffers (name/folder normalization, Steam path, stat).
  pub match_scratch: MatchScratch,
  /// Exec-path buffers (cmdline, args, environ, `/proc` path).
  pub exec_scratch: ExecScratch,
}

/// Consecutive low-occupancy ticks before the slot list is compacted.
///
/// Long enough that a momentary spike never triggers a realloc, short
/// enough that a sustained drop (build finished, game exited) releases
/// the surplus within ~40 s at the base cadence.
///
/// Read by the `/proc` sweep and its tests, so it is dead weight on
/// targets that list processes through `sysinfo`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const SLOT_LOW_TICKS: u8 = 8;

// Per-pid allocation reuse for the scan tick: `read_exec` intermediates
// live in caller-owned `ExecScratch` buffers (one set per thread) and
// output strings reuse the destination `Exec` slot's capacity.
#[cfg(target_os = "linux")]
pub fn read_exec_into(pid: u64, exec: &mut Exec, scratch: &mut ExecScratch) -> bool {
  const MAX_CMDLINE_BYTES: u64 = 64 * 1024;
  use std::fmt::Write as _;
  scratch.path.clear();
  // `write!` into a cleared buffer reuses it; the only failure mode is
  // OOM (fmt::Error), matching the old `format!` behavior if allocation
  // fails.
  if write!(scratch.path, "/proc/{pid}/cmdline").is_err() {
    return false;
  }
  // NOTE: no metadata size check here — /proc files report st_size 0
  // despite having content; an early `len() == 0` return would skip
  // EVERY process (total detection blindness).
  let Ok(file) = std::fs::File::open(&scratch.path) else {
    return false;
  };
  scratch.cmdline.clear();
  use std::io::Read;
  if file
    .take(MAX_CMDLINE_BYTES + 1)
    .read_to_end(&mut scratch.cmdline)
    .is_err()
  {
    return false;
  }
  if scratch.cmdline.is_empty() {
    return false;
  }
  scratch
    .cmdline
    .truncate(usize::try_from(MAX_CMDLINE_BYTES).unwrap_or(usize::MAX));
  // Truncation may split a multibyte char: back off to the boundary
  // in one step (never rescan: `valid_up_to` is the split point).
  if let Err(err) = std::str::from_utf8(&scratch.cmdline) {
    scratch.cmdline.truncate(err.valid_up_to());
  }
  let Ok(text) = std::str::from_utf8(&scratch.cmdline) else {
    return false;
  };
  let mut cmd_iter = text.split('\0');
  let cmd_path = cmd_iter.next().unwrap_or("");
  // Output strings reuse the destination slot's capacity: `path` is
  // cleared and refilled in place; the old args buffer (if any) is
  // returned to the scratch for the next pid, so the two buffers trade
  // places instead of reallocating.
  exec.pid = pid;
  exec.path.clear();
  exec.path.push_str(cmd_path);
  scratch.args.clear();
  for (index, arg) in cmd_iter.enumerate() {
    if index > 0 {
      scratch.args.push(' ');
    }
    scratch.args.push_str(arg);
  }
  let previous = exec.arguments.take();
  if !scratch.args.is_empty() {
    exec.arguments = Some(std::mem::take(&mut scratch.args));
  }
  if let Some(mut old) = previous {
    old.clear();
    scratch.args = old;
  }
  true
}

/// Whether a database `os` tag applies on this build (unknown platforms match all).
pub(crate) fn os_matches(os: &str) -> bool {
  match std::env::consts::OS {
    "windows" => os == "win32",
    "macos" => os == "darwin",
    "linux" => os == "linux",
    _ => true,
  }
}

/// Read `SteamAppId` from `/proc/<pid>/environ` (set by Steam/Proton for
/// every game process it launches, including non-Steam shortcuts).
/// Callers read this lazily: environ is the most expensive read of the
/// scan, so only misses pay for it.
///
/// The scratch supplies both buffers — `path` for the `/proc/<pid>` prefix
/// and `environ` for the body — so a miss (every non-Steam process) costs
/// no allocation of its own, and a hit keeps the buffer sized for the
/// next one.
#[cfg(target_os = "linux")]
pub(crate) fn read_steam_app_id(pid: u64, scratch: &mut ExecScratch) -> Option<String> {
  // Bounded: 256 KiB covers any legitimate environ (SteamAppId lives
  // near the front); larger means hostile or broken, and reading it
  // whole per new pid would multiply per tick.
  const MAX_ENVIRON_BYTES: u64 = 256 * 1024;

  // NOTE: no metadata size check — /proc files report st_size 0 (see above).
  use std::fmt::Write as _;
  use std::io::Read;

  // Same buffer the exec reader reuses: cleared and refilled per pid.
  scratch.path.clear();
  write!(scratch.path, "/proc/{pid}/environ").ok()?;
  let file = std::fs::File::open(&scratch.path).ok()?;

  scratch.environ.clear();
  file
    .take(MAX_ENVIRON_BYTES + 1)
    .read_to_end(&mut scratch.environ)
    .ok()?;
  scratch
    .environ
    .truncate(usize::try_from(MAX_ENVIRON_BYTES).unwrap_or(usize::MAX));
  for entry in scratch.environ.split(|b| *b == 0) {
    if let Some(id) = entry.strip_prefix(b"SteamAppId=")
      && let Ok(id) = std::str::from_utf8(id)
    {
      let id = id.trim();
      if !id.is_empty() {
        return Some(id.to_string());
      }
    }
  }
  None
}

/// No environ AppIds outside Linux (Steam matching there was already
/// limited to the exe/name paths).
///
/// The scratch is carried for a uniform signature with the Linux reader
/// above: the call-site hands its buffer down on every platform instead
/// of spelling the call out twice, and none of these targets has an
/// environ to read into it.
#[cfg(not(target_os = "linux"))]
pub(crate) fn read_steam_app_id(_pid: u64, _scratch: &mut ExecScratch) -> Option<String> {
  None
}

/// Steam AppId from a process command line (`reaper SteamLaunch
/// AppId=4508340 ...`): fallback when `/proc/<pid>/environ` is
/// unreadable. Sandboxed Proton runtimes (pressure-vessel/bwrap) hide
/// environ from service contexts while cmdline stays readable — without
/// this, every such game is invisible to automatic detection. Same trust
/// as environ (both launcher-provided, display-only use): the token must
/// stand alone (`AppId=` at a word boundary, followed by digits).
pub fn app_id_from_args(arguments: Option<&str>) -> Option<&str> {
  const TOKEN: &str = "AppId=";
  let args = arguments?;
  let mut rest = args;
  while let Some(pos) = rest.find(TOKEN) {
    let boundary = pos == 0
      || rest[..pos]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric());
    rest = &rest[pos + TOKEN.len()..];
    if !boundary {
      continue;
    }
    // Borrow the digit run instead of collecting it: ASCII digits are
    // single-byte, so the byte count is always a char boundary.
    let len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if len > 0 {
      return Some(&rest[..len]);
    }
  }
  None
}

/// Whether a process is currently suspended (`T` state: SIGSTOP'd, e.g. a
/// paused game). Suspended games show a frozen frame (or nothing) — they
/// are not being played, so matches on them are discarded and the scan
/// treats them as absent (clears). Checked lazily, only for matched
/// processes: one tiny `stat` read per hit, never per scan.
///
/// `stat` is the caller's reuse buffer (see [`MatchScratch::stat`]):
/// cleared and refilled here, so a hit costs no allocation of its own.
#[cfg(target_os = "linux")]
pub fn is_suspended(pid: u64, stat: &mut String) -> bool {
  // Capped like every other `/proc` read (`stat` is tiny, but the cap
  // documents the rule has no exceptions).
  use std::io::Read;
  stat.clear();
  std::fs::File::open(format!("/proc/{pid}/stat"))
    .and_then(|file| file.take(8 * 1024).read_to_string(stat).map(|_| ()))
    .ok()
    .and_then(|()| parse_stat_state(stat))
    .is_some_and(|state| state == 'T' || state == 't')
}

/// Stubbed on non-Linux (Steam matching there is already limited).
#[cfg(not(target_os = "linux"))]
pub(crate) fn is_suspended(_pid: u64, _stat: &mut String) -> bool {
  false
}

/// Process state from `/proc/<pid>/stat`: the field right after the last
/// `)` (comm may itself contain spaces and parens). `None` when unreadable
/// or malformed — never counted as suspended. Linux-only like its sole
/// caller: without `/proc` there is nothing to parse.
#[cfg(target_os = "linux")]
#[inline]
pub fn parse_stat_state(stat: &str) -> Option<char> {
  stat
    .rfind(')')
    .and_then(|end| stat[end + 1..].split_whitespace().next())
    .and_then(|state| state.chars().next())
}

/// Lowercase-trim a name for map keys and lookups. ASCII-only by
/// design: every lookup side lowercases ASCII too, and the AC automaton
/// matches `ascii_case_insensitive` — one consistent rule for a
/// Windows-centric database, instead of half the paths folding Unicode
/// and the other half not.
///
/// Punctuation forbidden in Windows filenames (`: ? " < > | * / \`)
/// is dropped (via a space, runs collapsed): no real folder or exe stem
/// can ever contain those characters, so a DB title like `Name:
/// Subtitle` still matches its on-disk `Name Subtitle` folder — while
/// the multi-word/length gate keeps generic collisions out exactly as
/// before.
#[inline]
pub fn normalize_name(name: &str) -> String {
  const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
  name
    .trim()
    .to_ascii_lowercase()
    .replace(&FORBIDDEN[..], " ")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

/// Punctuation forbidden in Windows filenames (see [`normalize_name`]).
/// Shared by the allocating original and the reuse variants below.
const FORBIDDEN_NAME_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Reusable buffers for the match path (see `tick2_tests`): `lowered`
/// holds the lowercased process path per miss, `norm_out` the normalized
/// folder per component. One set per thread, owned by the calling loop
/// (scan loop, EXEC dispatch) — cleared and refilled, never rebuilt per
/// process. Borrowing contract: `lowered` and `norm_out` are disjoint
/// fields, so a shared `lowered` borrow coexists with an exclusive
/// `norm_out` refill across the folder loops.
#[derive(Default)]
pub struct MatchScratch {
  pub lowered: String,
  pub norm_out: String,
  /// Canonical Steam path for the prefix check (lowercase, `/`
  /// separators, leading `/`): reused across processes so a miss costs
  /// no allocation. Only touched on the Steam-library tail.
  pub steam_path: String,
  /// `/proc/<pid>/stat` body for the suspended check: tiny, but read for
  /// every aux hit (0-3 per tick), so it pays to reuse the buffer rather
  /// than build a fresh `String` (plus the path) per hit. Cleared before
  /// every read so a failed one can never leave a stale line to parse.
  pub stat: String,
}

/// Refill `out` with the normalized form of `name`, reusing both
/// buffers: byte-identical to [`normalize_name`]. Single pass —
/// lowercase into `lower`, then split on forbidden/whitespace (runs
/// collapse, leading/trailing trimmed by construction) straight into
/// `out`, with no temporaries — no `replace` String, no pieces `Vec`,
/// no join.
pub fn normalize_name_into(name: &str, lower: &mut String, out: &mut String) {
  lower.clear();
  lower.extend(name.trim().chars().map(|c| c.to_ascii_lowercase()));
  normalize_lowered_into(lower, out);
}

/// Refill `out` with the normalized form of already-lowercase `name`
/// (the hot path feeds `&lowered`): skips the lowercase stage entirely.
/// Byte-identical to [`normalize_name`] on lowercase input.
pub fn normalize_lowered_into(name: &str, out: &mut String) {
  out.clear();
  let mut first = true;
  for word in name
    .split(|c: char| c.is_whitespace() || FORBIDDEN_NAME_CHARS.contains(&c))
    .filter(|word| !word.is_empty())
  {
    if !first {
      out.push(' ');
    }
    first = false;
    out.push_str(word);
  }
}

/// Refill `out` with the de-dotted normalized form of already-lowercase
/// `name`: byte-identical to [`normalize_name`] applied to `dedot(name)`
/// (dots become spaces before the same collapse).
pub fn normalize_lowered_dedotted_into(name: &str, out: &mut String) {
  out.clear();
  let mut first = true;
  for word in name
    .split(|c: char| c.is_whitespace() || c == '.' || FORBIDDEN_NAME_CHARS.contains(&c))
    .filter(|word| !word.is_empty())
  {
    if !first {
      out.push(' ');
    }
    first = false;
    out.push_str(word);
  }
}

/// Conservative gate for the exe-stem fallback: exact, multi-word names with
/// a minimum length. Keeps generic stems (`fish`, `steam`, `game`, `reaper`)
/// from ever matching same-named DB entries.
#[inline]
pub fn name_matchable(normalized: &str) -> bool {
  normalized.contains(' ') && normalized.chars().count() >= 6
}

/// Executable stem of an already-normalized (`/`-separated, lowercase) path,
/// without extension: `/games/how to fish.exe` -> `how to fish`. Borrowed:
/// callers only compare it against the map.
#[inline]
pub fn exe_stem(normalized_path: &str) -> &str {
  let base = normalized_path
    .rsplit('/')
    .next()
    .unwrap_or(normalized_path);
  match base.rfind('.') {
    Some(dot) if dot > 0 => &base[..dot],
    _ => base,
  }
}

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

/// Cap on remembered application ids per boot.
///
/// The set exists only to log a first sighting once, not to remember every
/// id a long-lived daemon ever saw: a daemon that detects a thousand
/// distinct games over weeks would otherwise grow this set forever.
const MAX_SEEN_IDS: usize = 1024;

/// First sightings this boot: entries of `detected` not yet in `seen`
/// (which is updated in place). The scan loop logs these at INFO so a
/// silent daemon is distinguishable from a blind one without a debug
/// build; the bridge still owns publish/dedup logging downstream.
///
/// `seen` is bounded: past [`MAX_SEEN_IDS`] it is dropped whole rather
/// than growing without limit, so a repeat INFO may surface after enough
/// id churn. That is the accepted trade for constant memory — the log
/// line is diagnostic, never state.
pub fn first_sightings<'a>(
  seen: &mut HashSet<String>,
  detected: &'a [ScannedHit],
) -> Vec<&'a ScannedHit> {
  if seen.len() >= MAX_SEEN_IDS {
    seen.clear();
  }
  let mut first = Vec::new();
  for game in detected {
    // `contains` first: the common case is a game already seen this boot,
    // and building the key only for `insert` to discard it allocated for
    // nothing.
    if seen.contains(game.entry.id.as_ref()) {
      continue;
    }
    seen.insert(game.entry.id.to_string());
    first.push(game);
  }
  first
}

/// Drop scan results whose application id is ignored, preserving order.
/// Pure: an ignored-only scan yields an empty vec, which the scan thread
/// already turns into the normal null event (clear). Tested below.
pub fn apply_ignore_list(
  detected: Vec<ScannedHit>,
  ignored_ids: &HashSet<String>,
) -> Vec<ScannedHit> {
  if ignored_ids.is_empty() {
    return detected;
  }
  detected
    .into_iter()
    .filter(|game| {
      let id: &str = &game.entry.id;
      !ignored_ids.contains(id)
    })
    .collect()
}

/// Shared tail of every direct path hit (native or Proton): when the
/// database declares `arguments` for an executable, the process command
/// line must contain them (parity with arrpc/pog5-rsrpc, e.g. TF2
/// `-game tf`); then the suspended check + timestamp stamp via
/// `live_or_none`.
///
/// `stat` is the shared reuse buffer for the suspension read.
fn finish_direct_hit(
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

/// Steam's non-Steam shortcut range: ids Steam itself assigns when a
/// shortcut is added carry the high bit (`crc32(exe + name) | 0x80000000`,
/// e.g. 2532755798 for "How to Fish" — verified against a live
/// `shortcuts.vdf`); real store ids are small. Unknown to the DB +
/// shortcut-range means a self-identified non-Steam game.
#[inline]
pub(crate) fn is_shortcut_id(app_id: &str) -> bool {
  app_id.parse::<u32>().is_ok_and(|id| id & 0x8000_0000 != 0)
}

/// Authoritative aux lookup: `SteamAppId` (store games with empty
/// `executables`, legit Steam). Runs before every fuzzy heuristic — a
/// store id beats path guessing. Custom overrides participate as a
/// linear fallback (they are user-sized, not DB-sized): a custom entry
/// carrying only a steam distributor id matches here, since the map
/// only indexes the main list. Canonical map hits always win.
pub fn match_steam_id(
  steam_app_id: Option<&str>,
  pid: u64,
  steam_map: &SortedIndex,
  detectable_list: &[Arc<ScannedEntry>],
  custom: &[Arc<ScannedEntry>],
  source: DetectSource,
  stat: &mut String,
) -> Option<ScannedHit> {
  let appid = steam_app_id?;
  if let Some(&idx) = steam_map.get(appid) {
    let obj = detectable_list.get(idx)?;
    tracing::debug!(
      "[Process Scanner] Steam match: {} (appid {})",
      obj.name,
      appid
    );
    return live_or_none(obj, pid, source, stat);
  }
  // Custom override with a bare steam SKU (no executables to index):
  // the provenance travels through — a library-inherited app id matched
  // here still reports the library path that supplied it.
  for obj in custom {
    let sku_match = obj.steam_ids.iter().any(|id| {
      let sid: &str = id;
      sid == appid
    });
    if sku_match {
      tracing::debug!(
        "[Process Scanner] Steam match (custom): {} (appid {})",
        obj.name,
        appid
      );
      return live_or_none(obj, pid, source, stat);
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
    && let Some(&idx) = name_map.get(stem)
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
      && let Some(&idx) = name_map.get(folder)
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
      && let Some(&idx) = name_map_nodot.get(folder)
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

/// Process working directory via `/proc/<pid>/cwd` (one readlink, no file
/// content). Proton launches with the game dir as cwd, so joining it with
/// a bare exe reconstructs the install path the DB knows (DOOM Eternal
/// case). Read lazily: only bare-exe misses pay for it.
#[cfg(target_os = "linux")]
pub(crate) fn read_cwd(pid: u64) -> Option<String> {
  let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
  Some(cwd.to_str()?.to_ascii_lowercase())
}

/// No cwd outside Linux (Steam matching there is already limited).
#[cfg(not(target_os = "linux"))]
pub(crate) fn read_cwd(_pid: u64) -> Option<String> {
  None
}

impl ProcessServer {
  /// Main-DB half of [`ProcessServer::ac_probe`]: every path variant is
  /// probed here before any variant reaches custom overrides, so a main
  /// pattern always beats a custom one regardless of 64-bit stripping.
  fn main_probe(
    &self,
    reversed_path: &str,
    bundle: &DetectablesBundle,
  ) -> Option<(Arc<ScannedEntry>, usize)> {
    let mat = bundle.ac.find(reversed_path)?;
    let pair = bundle.indexes[mat.pattern().as_usize()].resolve();
    Some((bundle.list[pair.0].clone(), pair.1))
  }

  /// Custom-overrides half of [`ProcessServer::ac_probe`].
  fn custom_probe(
    &self,
    reversed_path: &str,
    bundle: &DetectablesBundle,
  ) -> Option<(Arc<ScannedEntry>, usize)> {
    let custom_ac = bundle.custom_ac.as_ref()?;
    let mat = custom_ac.find(reversed_path)?;
    let pair = bundle.custom_indexes[mat.pattern().as_usize()].resolve();
    Some((bundle.custom[pair.0].clone(), pair.1))
  }

  /// Proton fallback probe (main DB `win32` entries on Linux): same shape
  /// as [`ProcessServer::ac_probe`], consulted only after the native
  /// patterns, user overrides and the authoritative Steam AppId all miss.
  /// Empty automaton off-Linux, so this is a cheap `None` there.
  pub fn proton_probe(
    &self,
    reversed_path: &str,
    bundle: &DetectablesBundle,
  ) -> Option<(Arc<ScannedEntry>, usize)> {
    let automaton = bundle.proton_ac.as_ref()?;
    let mat = automaton.find(reversed_path)?;
    let pair = bundle.proton_indexes[mat.pattern().as_usize()].resolve();
    Some((bundle.list[pair.0].clone(), pair.1))
  }

  /// Shared variant loop: try `path` plus its 64-bit-stripped variants
  /// against the native (`proton = false`) or Proton (`proton = true`)
  /// automaton. Precedence is per automaton, not per variant: every
  /// variant is probed against main first, then every variant against
  /// custom — so a main pattern always beats a custom one even when
  /// only a stripped variant collides (e.g. main `wow.exe` vs custom
  /// `wow64.exe`).
  fn probe_variants(
    &self,
    path: &str,
    variant_bufs: &mut [String; 5],
    reversed_path: &mut String,
    bundle: &DetectablesBundle,
    proton: bool,
  ) -> Option<(Arc<ScannedEntry>, usize)> {
    let variant_count = path_variants_into(path, variant_bufs);
    // Automaton passes outer, variants inner (strict precedence). Without
    // user overrides there is no custom automaton: run a single pass
    // instead of probing a guaranteed-`None` second automaton per variant.
    let passes: usize = if proton || bundle.custom_ac.is_none() {
      1
    } else {
      2
    };
    for pass in 0..passes {
      for variant in &variant_bufs[..variant_count] {
        reversed_path.clear();
        reversed_path.extend(variant.chars().rev());
        let found = if proton {
          self.proton_probe(reversed_path, bundle)
        } else if pass == 0 {
          self.main_probe(reversed_path, bundle)
        } else {
          self.custom_probe(reversed_path, bundle)
        };
        if found.is_some() {
          return found;
        }
      }
    }
    None
  }

  /// Classify one enumerated process, cheapest source first:
  /// native AC, bare-exe+cwd, authoritative Steam AppId, Proton (`win32`)
  /// AC, Steam install-dir, then the stem/folder heuristics — except when
  /// the launcher supplied a shortcut-range AppId (Steam-assigned
  /// non-Steam id): then stem/folder run before the install-dir. Lazy `/proc` reads (cwd,
  /// environ, stat) happen only on misses/hits respectively — never for
  /// the whole table. Extracted from the scan loop for reuse and testing;
  /// the loop itself just maps over it.
  #[hotpath::measure]
  pub fn match_process(
    &self,
    process: &Exec,
    bundle: &DetectablesBundle,
    variant_bufs: &mut [String; 5],
    reversed_path: &mut String,
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
    if self
      .exclusions
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .is_excluded(&basename)
    {
      tracing::debug!("[Process Scanner] Excluded process, skipping: {basename}");
      return None;
    }

    // OBS binaries ship lowercase; the flag only feeds an unconsumed
    // observer callback, so original-case matching is exact enough.
    if !*obs_open && (process_path.contains("obs64") || process_path.contains("streamlabs")) {
      *obs_open = true;
    }

    // Aho-Corasick matching against the path and its 64-bit-stripped
    // variants (so `wow64.exe` also matches a `wow.exe` pattern, like
    // arrpc/pog5-rsrpc). First hit in variant order wins.
    let mut found = self.probe_variants(&process_path, variant_bufs, reversed_path, bundle, false);
    // Which probe won (native path vs cwd-joined): tracked alongside
    // `found` because both funnel into one tail call below.
    let mut found_source = DetectSource::Automaton;

    // Proton bare-exe probe (DOOM Eternal case): argv[0] without
    // directories plus the process cwd often reconstructs the install
    // path the DB knows. Only for bare exes (paths with directories
    // already had their full match above); one readlink per miss.
    if found.is_none()
      && let Some(exe) = bare_exe(&process_path)
      && let Some(cwd) = read_cwd(process.pid)
    {
      let candidate = format!("{cwd}/{exe}");
      tracing::debug!(
        "[Process Scanner] Bare exe, probing cwd-joined path for pid {}",
        process.pid
      );
      found = self.probe_variants(&candidate, variant_bufs, reversed_path, bundle, false);
      if found.is_some() {
        found_source = DetectSource::CwdJoined;
        tracing::debug!("[Process Scanner] Cwd match for pid {}", process.pid);
      }
    }

    let (obj, exe_index) = match found {
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
        // Authoritative Steam AppId first: a store id beats every fuzzy
        // path heuristic below.
        if let Some(hit) = match_steam_id(
          app_id.as_deref(),
          process.pid,
          &bundle.steam_map,
          &bundle.list,
          &bundle.custom,
          DetectSource::SteamAppId,
          &mut scratch.match_scratch.stat,
        ) {
          return Some(hit);
        }
        // An AppId the launcher gave us but the database doesn't know needs
        // a second look: Steam itself assigns high-bit-set ids
        // (e.g. 2532755798) to non-Steam shortcuts, while real store ids
        // are small. A shortcut-range id is a self-identified non-Steam
        // game, so its own exe/folder name beats install location; a
        // small unknown id is a real game missing from the DB, where
        // location (Steam's ground truth) still beats name guessing.
        let non_steam = app_id.as_deref().is_some_and(is_shortcut_id);
        if non_steam
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
        // too generic for the stem/folder heuristics.
        if let Some((obj, exe_index)) =
          self.probe_variants(&process_path, variant_bufs, reversed_path, bundle, true)
        {
          return finish_direct_hit(
            &obj,
            exe_index,
            process,
            DetectSource::ProtonAutomaton,
            &mut scratch.match_scratch.stat,
          );
        }
        // Steam's own word: the process runs under a known install dir,
        // so it inherits that entry's AppId. Beats name guessing below,
        // loses to a DB-declared path above.
        if let Some(library_appid) =
          self.steam_prefix_app_id_into(lowered, &mut scratch.match_scratch.steam_path)
          && let Some(hit) = match_steam_id(
            Some(&library_appid),
            process.pid,
            &bundle.steam_map,
            &bundle.list,
            &bundle.custom,
            DetectSource::SteamLibrary,
            &mut scratch.match_scratch.stat,
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
    self
      .main_probe(reversed_path, bundle)
      .or_else(|| self.custom_probe(reversed_path, bundle))
  }
}

// Per-pid allocation reuse for the scan tick: `read_exec_into` refills
// caller-owned buffers and `process_list_into` reuses every `Exec` slot
// across ticks. Parity + no-realloc regression tests. Both helpers are
// Linux-only (`/proc`), so the module is too.
#[cfg(all(test, target_os = "linux"))]
mod reuse_tests {
  use super::*;
  use crate::server::ProcessServer;

  /// Own pid is always readable: deterministic fixture, no /proc guessing.
  fn self_pid() -> u64 {
    u64::from(std::process::id())
  }

  /// `read_exec_into` classifies exactly like `read_exec`: same presence,
  /// same pid, same path, same arguments.
  #[test]
  fn read_exec_into_matches_read_exec() {
    for pid in [self_pid(), 1] {
      let mut slot = Exec::default();
      let mut scratch = ExecScratch::default();
      let present = read_exec_into(pid, &mut slot, &mut scratch);
      match read_exec(pid) {
        None => assert!(!present, "into() must agree on unreadable pid {pid}"),
        Some(expected) => {
          assert!(present, "into() must agree on readable pid {pid}");
          assert_eq!(slot.pid, expected.pid);
          assert_eq!(slot.path, expected.path);
          assert_eq!(slot.arguments, expected.arguments);
        }
      }
    }
    // Out-of-range pid: both agree on absence without touching buffers.
    let mut slot = Exec::default();
    let mut scratch = ExecScratch::default();
    assert!(!read_exec_into(u64::MAX, &mut slot, &mut scratch));
    assert!(read_exec(u64::MAX).is_none());
  }

  /// A second call with the same pid must not reallocate: scratch pointers
  /// and capacities are stable, slot strings keep their buffers.
  #[test]
  fn read_exec_into_reuses_buffers() {
    let pid = self_pid();
    let mut slot = Exec::default();
    let mut scratch = ExecScratch::default();
    assert!(read_exec_into(pid, &mut slot, &mut scratch));
    let path_ptr = slot.path.as_ptr();
    let path_cap = slot.path.capacity();
    let cmd_ptr = scratch.cmdline.as_ptr();
    let cmd_cap = scratch.cmdline.capacity();
    assert!(read_exec_into(pid, &mut slot, &mut scratch));
    assert!(
      std::ptr::eq(path_ptr, slot.path.as_ptr()),
      "path buffer moved"
    );
    assert!(
      std::ptr::eq(cmd_ptr, scratch.cmdline.as_ptr()),
      "cmdline buffer moved"
    );
    assert!(slot.path.capacity() >= path_cap);
    assert!(scratch.cmdline.capacity() >= cmd_cap);
  }

  /// D5: `read_steam_app_id` reads environ into the scratch's own buffer
  /// and builds its path in the scratch's path buffer, so a miss — the
  /// common case, every non-Steam process — costs no allocation at all
  /// beyond the returned `Option`.
  ///
  /// environ is kilobytes and the only read of the scan that big: it
  /// runs once per newly-seen pid, but a fresh `Vec` (plus the `format!`
  /// of the path) was paid every time, with the buffer growing back up
  /// to its real size each call instead of keeping the capacity.
  ///
  /// The reuse check is about capacity, not pointers: `read_to_end`
  /// legitimately grows the buffer on the first call, so the assertion is
  /// that the second call neither reallocates nor shrinks it. `path` is
  /// the stronger half — same `String`, refilled with `write!`.
  #[test]
  fn steam_app_id_read_reuses_the_exec_scratch() {
    // Self pid is deterministic (present, readable) and its own environ
    // is irrelevant: what matters is that both buffers survive the call.
    // Whether it carries a SteamAppId is environment-dependent, so the
    // assertions below are the reuse contract, not the lookup result.
    let pid = self_pid();
    let mut scratch = ExecScratch::default();

    let _ = read_steam_app_id(pid, &mut scratch);
    let path_ptr = scratch.path.as_ptr();
    let path_cap = scratch.path.capacity();
    let env_ptr = scratch.environ.as_ptr();
    let env_cap = scratch.environ.capacity();

    let _ = read_steam_app_id(pid, &mut scratch);

    assert!(
      std::ptr::eq(path_ptr, scratch.path.as_ptr()),
      "path buffer moved: the environ read must reuse it"
    );
    assert!(
      std::ptr::eq(env_ptr, scratch.environ.as_ptr()),
      "environ buffer moved: the read must reuse it"
    );
    assert!(scratch.path.capacity() >= path_cap, "path buffer shrank");
    assert!(scratch.environ.capacity() >= env_cap, "environ shrank");
    // The reuse target is the scratch, so it must have been touched: an
    // implementation that falls back to its own buffers leaves these
    // empty while still passing every pointer check above.
    assert_eq!(
      scratch.path,
      format!("/proc/{pid}/environ"),
      "the read must build its path in the scratch"
    );
    assert!(
      !scratch.environ.is_empty(),
      "the read must land its bytes in the scratch"
    );
  }

  /// `process_list_into` reuses the Vec backing and every slot across
  /// ticks: same allocation, refreshed contents. The first-slot pointer
  /// check is conditional on pid stability: if a process exits between
  /// the two back-to-back sweeps, readdir order may shift and the slot
  /// legitimately holds another pid. The backing-pointer check is
  /// conditional on capacity: growth past the first high-water mark may
  /// reallocate, which is correct, not a reuse bug. Surplus slots past
  /// `filled` are retained (high-water), so assertions only read the
  /// filled prefix.
  #[test]
  fn process_list_into_reuses_slots_across_ticks() {
    let mut processes = Vec::new();
    let mut scratch = ExecScratch::default();
    let filled =
      ProcessServer::process_list_into(&mut processes, &mut scratch).expect("first tick lists");
    assert!(filled > 0, "some process must be visible");
    assert!(filled <= processes.len());
    assert!(
      processes[..filled]
        .iter()
        .all(|e| e.pid > 0 && !e.path.is_empty())
    );
    let backing_ptr = processes.as_ptr();
    let backing_cap = processes.capacity();
    let (first_pid, first_path_ptr, first_path_cap) = (
      processes[0].pid,
      processes[0].path.as_ptr(),
      processes[0].path.capacity(),
    );
    let filled2 =
      ProcessServer::process_list_into(&mut processes, &mut scratch).expect("second tick lists");
    assert!(filled2 > 0);
    assert!(filled2 <= processes.len());
    if processes.len() <= backing_cap {
      assert!(
        std::ptr::eq(backing_ptr, processes.as_ptr()),
        "Vec backing moved"
      );
    }
    if processes[0].pid == first_pid {
      assert!(
        std::ptr::eq(first_path_ptr, processes[0].path.as_ptr()),
        "slot string buffer moved"
      );
      assert!(processes[0].path.capacity() >= first_path_cap);
    }
    // High-water retention: the second sweep never shrinks the Vec, so a
    // later rise reuses surplus slot buffers instead of reallocating.
    assert!(processes.len() >= filled);
    assert!(processes.len() >= filled2);
  }
}

// Match-path allocation reuse: the into-variants refill caller-owned
// buffers with byte-identical outputs (corpus below covers case,
// punctuation, dots, whitespace, empties and non-ASCII).
#[cfg(test)]
mod tick2_tests {
  use super::*;

  /// Corpus covering case, forbidden punctuation, dots, whitespace
  /// runs, empties, separators, `>` prefix and non-ASCII.
  const CORPUS: &[&str] = &[
    "Game Name",
    "Name: Subtitle",
    "R.E.P.O. Ghost Haul",
    "Q.U.B.E.",
    "Mr. Bomber",
    "a  b   c",
    "",
    "   ",
    "a/b\\c",
    ">game",
    "/",
    "fish",
    "École: LÉGENDE",
    "MECCHA CHAMELEON",
    "PenguinHotel-Win64-Shipping.exe",
    "  padded  ",
    "a.b.c",
    "...",
  ];

  /// `normalize_name_into` reproduces `normalize_name` byte-for-byte.
  #[test]
  fn normalize_into_matches_normalize() {
    let mut lower = String::new();
    let mut out = String::new();
    for input in CORPUS {
      normalize_name_into(input, &mut lower, &mut out);
      assert_eq!(&out, &normalize_name(input), "mismatch for {input:?}");
    }
  }

  /// On already-lowercase input, the lowered variant (no lowercase
  /// stage) matches too — this is the hot path (`&lowered`).
  #[test]
  fn normalize_lowered_into_matches_on_lowercase_input() {
    let mut out = String::new();
    for input in CORPUS {
      let lowered = input.to_ascii_lowercase();
      normalize_lowered_into(&lowered, &mut out);
      assert_eq!(&out, &normalize_name(input), "mismatch for {input:?}");
    }
  }

  /// The dedotted variant matches `normalize_name(&dedot(_))`: dots
  /// become spaces, runs collapse, same as replace-then-normalize.
  #[test]
  fn normalize_lowered_dedotted_into_matches_dedot_then_normalize() {
    use crate::bundle::dedot;
    let mut out = String::new();
    for input in CORPUS {
      let lowered = input.to_ascii_lowercase();
      normalize_lowered_dedotted_into(&lowered, &mut out);
      assert_eq!(
        &out,
        &normalize_name(&dedot(input)),
        "mismatch for {input:?}"
      );
    }
  }

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
      aliases: Box::new([]),
    });
    let steam_map = SortedIndex::build(vec![("12345".into(), 0)]);
    let hit = match_steam_id(
      Some("12345"),
      u64::from(std::process::id()),
      &steam_map,
      &[entry],
      &[],
      DetectSource::SteamAppId,
      &mut String::new(),
    )
    .expect("appid hits");
    assert_eq!(hit.source, DetectSource::SteamAppId);
  }

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
      aliases: Box::new([]),
    });
    let empty_map = SortedIndex::build(Vec::new());
    let hit = match_steam_id(
      Some("999"),
      u64::from(std::process::id()),
      &empty_map,
      &[],
      &[entry],
      DetectSource::SteamLibrary,
      &mut String::new(),
    )
    .expect("custom sku hits");
    assert_eq!(hit.source, DetectSource::SteamLibrary);
  }

  /// Scratch buffers are stable across calls: no reallocations on
  /// repeated use.
  #[test]
  fn match_scratch_buffers_are_stable() {
    let mut scratch = MatchScratch::default();
    let mut lower = String::new();
    normalize_name_into("Some Game: Title", &mut lower, &mut scratch.norm_out);
    normalize_lowered_into("some game title", &mut scratch.norm_out);
    let out_ptr = scratch.norm_out.as_ptr();
    let out_cap = scratch.norm_out.capacity();
    let lower_ptr = lower.as_ptr();
    normalize_name_into("Other: Name Here", &mut lower, &mut scratch.norm_out);
    normalize_lowered_into("other name here", &mut scratch.norm_out);
    assert!(
      std::ptr::eq(out_ptr, scratch.norm_out.as_ptr()),
      "norm_out moved"
    );
    assert!(std::ptr::eq(lower_ptr, lower.as_ptr()), "lower moved");
    assert!(scratch.norm_out.capacity() >= out_cap);
  }

  /// D6: the suspended check parses `/proc/<pid>/stat` into the scratch's
  /// own buffer instead of building a fresh `String` per hit.
  ///
  /// The read is tiny (`stat` is a few hundred bytes) but it runs for
  /// every aux hit — 0 to 3 per tick — and the previous code paid two
  /// allocations each time: a `String::new()` plus the `format!` of the
  /// path. Letting the scratch own the buffer makes repeat checks free.
  ///
  /// The empty-buffer case matters as much as the steady state: a check
  /// that fails to fill the scratch (unreadable pid) must leave it clear,
  /// not leave last tick's bytes behind for the next parse to misread.
  #[cfg(target_os = "linux")]
  #[test]
  fn stat_scratch_is_reused_across_suspension_checks() {
    let pid = u64::from(std::process::id());
    let mut scratch = MatchScratch::default();

    // A readable pid fills the scratch; that is the reuse target.
    let _ = is_suspended(pid, &mut scratch.stat);
    assert!(
      !scratch.stat.is_empty(),
      "the check must parse into the scratch buffer"
    );
    let stat_ptr = scratch.stat.as_ptr();
    let stat_cap = scratch.stat.capacity();

    for _ in 0..3 {
      let _ = is_suspended(pid, &mut scratch.stat);
      assert!(
        std::ptr::eq(stat_ptr, scratch.stat.as_ptr()),
        "stat buffer moved"
      );
      assert!(scratch.stat.capacity() >= stat_cap, "stat buffer shrank");
    }

    // An unreadable pid reuses the same buffer and clears it, so a later
    // check can never parse the previous pid's line.
    let _ = is_suspended(u64::MAX, &mut scratch.stat);
    assert!(
      std::ptr::eq(stat_ptr, scratch.stat.as_ptr()),
      "failed checks must not reallocate"
    );
    assert!(
      scratch.stat.is_empty(),
      "a failed read must clear the scratch, not leave stale bytes"
    );
  }
}

#[cfg(test)]
mod first_sightings_tests {
  use super::*;

  /// A detected hit for `id` (pid is irrelevant here).
  fn hit(id: &str) -> ScannedHit {
    ScannedHit {
      entry: Arc::new(ScannedEntry {
        id: id.into(),
        name: "Game".into(),
        executables: Box::new([]),
        steam_ids: Box::new([]),
        aliases: Box::new([]),
      }),
      pid: 1,
      start: 0,
      source: DetectSource::Automaton,
    }
  }

  /// The set bounds memory instead of remembering every id a long-lived
  /// daemon ever saw. Dropping it whole means a repeat INFO can surface
  /// after churn — the accepted trade for constant memory.
  #[test]
  fn seen_ids_stay_bounded_across_id_churn() {
    let mut seen: HashSet<String> = HashSet::new();
    // Well past the cap, in distinct batches so the clear is reached
    // repeatedly rather than once.
    for batch in 0..(MAX_SEEN_IDS * 3 / 64 + 2) {
      let batch: Vec<ScannedHit> = (0..64).map(|i| hit(&format!("game-{batch}-{i}"))).collect();
      let _ = first_sightings(&mut seen, &batch);
      assert!(
        seen.len() <= MAX_SEEN_IDS,
        "seen grew to {} past the cap of {MAX_SEEN_IDS}",
        seen.len()
      );
    }
  }

  /// An id already seen yields no first sighting, and does not allocate a
  /// key just to have the insert discard it.
  #[test]
  fn repeat_ids_are_not_first_sightings() {
    let mut seen: HashSet<String> = HashSet::new();
    let batch = vec![hit("a"), hit("b"), hit("a")];
    let first = first_sightings(&mut seen, &batch);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].entry.id.as_ref(), "a");
    assert_eq!(first[1].entry.id.as_ref(), "b");
    // Second pass over the same corpus: nothing is new.
    assert!(first_sightings(&mut seen, &batch).is_empty());
  }
}

// `/proc`-only: `process_list_into` is the Linux sweep and the slot
// release it drives is measured through it, so the module is gated the
// same way as `reuse_tests` above.
#[cfg(all(test, target_os = "linux"))]
mod slot_compaction_tests {
  use super::*;

  /// A pre-seeded slot list far past any plausible live process count, so
  /// every tick is a low-occupancy tick on any machine.
  fn surplus_list(count: usize) -> Vec<Exec> {
    (0..count)
      .map(|i| Exec {
        pid: i as u64 + 1,
        path: "p".repeat(64),
        arguments: Some("a".repeat(32)),
      })
      .collect()
  }

  /// Surplus slots survive below the threshold and are released exactly
  /// when it is reached: a spike must not be punished for one quiet
  /// moment, but a sustained drop must not pin slots forever.
  #[test]
  fn surplus_slots_are_released_only_after_sustained_low() {
    // 4x the threshold, and well past a realistic process count: the
    // premise (`live * 2 < slots`) holds on any machine this runs on.
    let surplus = SLOT_LOW_TICKS as usize * 512;
    let mut processes = surplus_list(surplus);
    let mut scratch = ExecScratch::default();

    let live = ProcessServer::process_list_into(&mut processes, &mut scratch).unwrap_or(0);
    if live * 2 >= surplus {
      // More live processes than the surplus (a huge host): the premise
      // fails, so this test has nothing to say. Never a false failure.
      return;
    }

    // Tick 1: the collapse is seen, nothing released yet.
    assert_eq!(processes.len(), surplus, "tick 1 must not release");
    assert_eq!(scratch.low_ticks, 1);

    // Ticks 2..threshold: still retained.
    for expected in 2..SLOT_LOW_TICKS {
      ProcessServer::process_list_into(&mut processes, &mut scratch).expect("tick lists");
      assert_eq!(
        scratch.low_ticks, expected,
        "counter climbs one per low tick"
      );
      assert_eq!(
        processes.len(),
        surplus,
        "surplus must survive under the threshold"
      );
    }

    // The threshold tick: released, and the counter resets. Released to
    // *this* tick's fill count, not the first tick's: the process count
    // drifts between ticks on a loaded host, so comparing against `live`
    // here would flake for a reason the test does not care about.
    let released_to =
      ProcessServer::process_list_into(&mut processes, &mut scratch).expect("release tick");
    assert_eq!(scratch.low_ticks, 0, "counter resets after a release");
    assert_eq!(
      processes.len(),
      released_to,
      "the list should sit at the live count"
    );
  }

  /// A healthy tick resets the counter, so spike-calm-spike-calm never
  /// compacts.
  #[test]
  fn low_ticks_reset_on_a_healthy_tick() {
    // Two pre-seeded slots: the sweep fills them and grows to exactly its
    // own live count, so occupancy is high by construction.
    let mut processes = vec![Exec::default(), Exec::default()];
    let mut scratch = ExecScratch {
      low_ticks: 5,
      ..ExecScratch::default()
    };

    if let Ok(filled) = ProcessServer::process_list_into(&mut processes, &mut scratch) {
      assert!(filled >= 2, "the sweep must fill its own slots");
      // The list covers every filled slot and sits at most one slot ahead
      // of the fill cursor: `process_list_into` pushes a slot *before*
      // reading it, so a read that fails for the last directory entry
      // leaves `len == filled + 1`. That last case is not a bug and not
      // rare under load (a process exiting between `readdir` and its
      // `read`), which is why the previous `len == filled` assertion
      // flaked.
      assert!(
        processes.len() >= filled && processes.len() <= filled + 1,
        "list sits at the live count (plus at most one unreadable slot), got len={} filled={filled}",
        processes.len()
      );
      assert_eq!(scratch.low_ticks, 0, "high occupancy resets the counter");
    }
  }
}
