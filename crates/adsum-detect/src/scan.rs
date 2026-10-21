//! Classification: one process in, an optional hit out.
//!
//! Cheapest source first (native AC, cwd-joined bare exe, authoritative
//! Steam AppId, Epic sandbox id or its `-epicapp` fallback, Proton AC,
//! install-dir prefix, stem/folder heuristics).
//! Lazy `/proc` reads happen only on misses/hits — never for the table.

use std::collections::HashSet;
use std::sync::Arc;

use crate::bundle::{DetectablesBundle, SortedIndex, bare_exe, path_variants_into};
use crate::server::ProcessServer;
use crate::types::{DetectSource, Exec, ScannedEntry, ScannedHit};
use adsum_types::AppId;

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

/// Both Epic launch tokens found in one pass over the command line.
#[derive(Debug, Default, PartialEq, Eq)]
struct EpicArgs<'a> {
  sandbox_id: Option<&'a str>,
  app_name: Option<&'a str>,
}

/// Find `-epicsandboxid=` and `-epicapp=` in one pass: each flag must start
/// an argument (line start or after whitespace) and its value runs to the
/// next whitespace; empty values are ignored.
fn epic_launch_args(arguments: Option<&str>) -> EpicArgs<'_> {
  const SANDBOX: &[u8] = b"-epicsandboxid=";
  const APP: &[u8] = b"-epicapp=";
  let Some(args) = arguments else {
    return EpicArgs::default();
  };
  let bytes = args.as_bytes();
  let mut found = EpicArgs::default();
  let mut at = 0;
  while at < bytes.len() {
    // Token boundary: the `-` has to start an argument (`x-epicapp=` is
    // the tail of some other flag, not this one).
    if bytes[at] != b'-' || (at != 0 && !bytes[at - 1].is_ascii_whitespace()) {
      at += 1;
      continue;
    }
    let rest = &bytes[at..];
    let (needle, is_sandbox) =
      if rest.len() >= SANDBOX.len() && rest[..SANDBOX.len()].eq_ignore_ascii_case(SANDBOX) {
        (SANDBOX, true)
      } else if rest.len() >= APP.len() && rest[..APP.len()].eq_ignore_ascii_case(APP) {
        (APP, false)
      } else {
        at += 1;
        continue;
      };
    // Borrow the value run instead of collecting it: the members are
    // single-byte ASCII, so the byte count is always a char boundary.
    let value_at = at + needle.len();
    let len = bytes[value_at..]
      .iter()
      .take_while(|byte| !byte.is_ascii_whitespace())
      .count();
    if len == 0 {
      at = value_at;
      continue;
    }
    let value = &args[value_at..value_at + len];
    if is_sandbox {
      found.sandbox_id.get_or_insert(value);
    } else {
      found.app_name.get_or_insert(value);
    }
    at = value_at + len;
  }
  found
}

/// Epic sandbox id (`-epicsandboxid=…`) from a process command line, the
/// distributor evidence Heroic/Epic leaves when the sandbox hides environ.
pub fn epic_id_from_args(arguments: Option<&str>) -> Option<&str> {
  epic_launch_args(arguments).sandbox_id
}

/// Epic app name (`-epicapp=…`) from a process command line: older
/// Legendary releases pass it without a sandbox id.
pub fn epic_app_from_args(arguments: Option<&str>) -> Option<&str> {
  epic_launch_args(arguments).app_name
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
/// `seen` is bounded: past `MAX_SEEN_IDS` it is dropped whole rather
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
/// line must contain them (parity with arrpc/pog5-adsum, e.g. TF2
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
fn argv_names_an_executable(process_path: &str, obj: &ScannedEntry) -> bool {
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

/// Whether `name` is the tail of `path` on a component boundary,
/// case-insensitive and with Windows separators folded.
fn names_tail(path: &str, name: &str) -> bool {
  // The launcher marker is arrpc's "the arguments carry the launcher"
  // flag, kept raw on the entry for the argument check in
  // `finish_direct_hit`; it is not part of the file name.
  let name = name.strip_prefix('>').unwrap_or(name);
  // A name that normalizes to a bare `/` is dropped at bundle build time
  // (it would match every path), so no hit can point at one — but a name
  // that names nothing must never name anything.
  if name.is_empty() {
    return false;
  }
  let path = path.as_bytes();
  let name = name.as_bytes();
  if let Some(at) = path.len().checked_sub(name.len()) {
    let tail = &path[at..];
    if tail.iter().zip(name).all(|(path_byte, name_byte)| {
      // A declared Windows separator is the `/` the path carries.
      let name_byte = if *name_byte == b'\\' {
        b'/'
      } else {
        *name_byte
      };
      path_byte.eq_ignore_ascii_case(&name_byte)
    }) {
      // What precedes the name has to be a separator — unless the name
      // carries its own leading one, which is the boundary itself (a
      // normalized pattern does). Bytes, not chars: the tail was matched
      // byte-for-byte above, so the split is already known to be aligned.
      let rest = &path[..at];
      if rest.is_empty() || rest.last() == Some(&b'/') || matches!(name.first(), Some(b'/' | b'\\'))
      {
        return true;
      }
    }
  }
  false
}

/// Whether `dir` occurs anywhere in `path` (case-insensitive, `\` folded),
/// so a near-miss component like `xgarrysmod` suppresses the fallback.
fn dir_present_in_path(path: &str, dir: &str) -> bool {
  #[inline]
  fn fold(byte: u8) -> u8 {
    if byte == b'\\' {
      b'/'
    } else {
      byte.to_ascii_lowercase()
    }
  }
  let dir = dir.as_bytes();
  if dir.is_empty() {
    return false;
  }
  path
    .as_bytes()
    .windows(dir.len())
    .any(|window| window.iter().zip(dir).all(|(p, d)| fold(*p) == fold(*d)))
}

/// Corroboration rule: exact tail, then the bare basename when the declared
/// directory is absent (Proton short launches), then the macOS bundle.
fn exe_names_path(path: &str, name: &str) -> bool {
  // The launcher marker is arrpc's "the arguments carry the launcher"
  // flag, kept raw on the entry for the argument check in
  // `finish_direct_hit`; it is not part of the file name.
  let name = name.strip_prefix('>').unwrap_or(name);
  // A name that normalizes to a bare `/` is dropped at bundle build time
  // (it would match every path), so no hit can point at one — but a name
  // that names nothing must never name anything.
  if name.is_empty() {
    return false;
  }
  // Full declared name first (multi-component entries launched with their
  // full relative path).
  if names_tail(path, name) {
    return true;
  }
  // Bare-basename fallback for short Proton/Heroic launches: this tier runs
  // only after a store id (Steam/Epic) already named the entry, so a generic
  // basename at the tail is corroboration, not standalone evidence.
  let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
  let dirpart = name.rsplit_once(['/', '\\']).map(|(d, _)| d).unwrap_or("");
  if basename != name && !basename.is_empty() {
    // A declared directory present as a substring (case-insensitive,
    // `\` folded) suppresses the fallback: the full name should have
    // corroborated, or it is a near miss.
    let dir_present = !dirpart.is_empty() && dir_present_in_path(path, dirpart);
    if !dir_present && names_tail(path, basename) {
      return true;
    }
  }
  names_a_bundle(path.as_bytes(), name.as_bytes())
}

/// The macOS bundle interior, which every bundle Apple has ever shipped
/// carries: `.../<Something>.app/Contents/MacOS/<binary>`.
const BUNDLE_INTERIOR: &[u8] = b"/contents/";

/// Whether `name` is a `.app` bundle standing at a component boundary with
/// that interior behind it.
///
/// A macOS entry names the bundle rather than the binary inside it:
/// Factorio declares `factorio.app` beside its Linux `factorio` and its
/// `factorio.exe`, but the process macOS runs is
/// `.../Factorio.app/Contents/MacOS/factorio` — the declared name is a
/// component in the middle of the path and never its tail, so the rule
/// above refuses the running game.
///
/// Requiring the interior is what keeps this from reopening the substring
/// hole the function exists to close: a name in the middle of a path names
/// nothing unless a bundle's own contents follow it, and
/// `.../factorio.app/something/factorio` still names nothing. 61 declared
/// executables across 56 entries in the bundled database are spelled this
/// way, every one of them `os: "darwin"`.
fn names_a_bundle(path: &[u8], name: &[u8]) -> bool {
  // Cheap enough to sit in front of the search: 56 of 24.299 entries
  // carry a `.app` name, and the four-byte compare is what keeps the
  // other ~24.000 off the walk below. This runs per declared executable
  // per probe, so it cannot afford to be unconditional.
  if name.len() < 4 || !name[name.len() - 4..].eq_ignore_ascii_case(b".app") {
    return false;
  }
  path.windows(name.len()).enumerate().any(|(at, window)| {
    let whole = window.iter().zip(name).all(|(path_byte, name_byte)| {
      let name_byte = if *name_byte == b'\\' {
        b'/'
      } else {
        *name_byte
      };
      path_byte.eq_ignore_ascii_case(&name_byte)
    });
    if !whole {
      return false;
    }
    // Same boundary rule as the tail case: a `/` before, or a name that
    // carries its own leading one.
    let fronted = at == 0 || path[at - 1] == b'/' || matches!(name.first(), Some(b'/' | b'\\'));
    fronted && starts_with_bundle_interior(&path[at + name.len()..])
  })
}

/// Whether `rest` opens with [`BUNDLE_INTERIOR`], folded case — the
/// default filesystem holds `Contents` with that spelling and plenty of
/// bundles arrive lowercased from a Windows-authored download.
fn starts_with_bundle_interior(rest: &[u8]) -> bool {
  rest.len() >= BUNDLE_INTERIOR.len()
    && rest[..BUNDLE_INTERIOR.len()].eq_ignore_ascii_case(BUNDLE_INTERIOR)
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
}

/// Authoritative aux lookup: `SteamAppId` (store games with empty
/// `executables`, legit Steam). Runs before every fuzzy heuristic — a
/// store id beats path guessing. Custom overrides participate as a
/// linear fallback (they are user-sized, not DB-sized): a custom entry
/// carrying only a steam distributor id matches here, since the map
/// only indexes the main list. Canonical map hits always win.
///
/// A hit is corroborated by the process path: the entry has to declare the
/// executable the process is running, matched on a component boundary. The
/// id says which game was launched, and Steam launches things on a game's
/// behalf with that game's id — its update script phase, the Proton prefix
/// setup, the shader pre-cache — so an id alone is evidence of work on the
/// game rather than of play. An entry declaring no executables is the case
/// this tier exists for and wins on the id alone: there is nothing to
/// corroborate against.
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
  if let Some(&idx) = lookup.steam_map.get(appid) {
    let obj = lookup.entries.get(idx)?;
    if !argv_names_an_executable(lookup.process_path, obj) {
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
      if !argv_names_an_executable(lookup.process_path, obj) {
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
  if let Some(&idx) = epic_map.get(epic_id) {
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
    variant_bufs: &mut [String; 5],
    reversed_path: &mut String,
    bundle: &DetectablesBundle,
    proton: bool,
  ) -> MatchOutcome {
    let variant_count = path_variants_into(path, variant_bufs);
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
      for variant in &variant_bufs[..variant_count] {
        reversed_path.clear();
        reversed_path.extend(variant.chars().rev());
        // One automaton's matches for this variant, in scan order, until
        // one names the string that was probed. Generic in the probe so
        // the three automatons — which yield different iterator types —
        // share the corroboration below instead of repeating it.
        let outcome = if proton {
          first_hit_naming_path(self.proton_probe(reversed_path, bundle), variant)
        } else if pass == 0 {
          first_hit_naming_path(self.main_probe(reversed_path, bundle), variant)
        } else {
          first_hit_naming_path(self.custom_probe(reversed_path, bundle), variant)
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
    let mut found = self.probe_variants(&process_path, variant_bufs, reversed_path, bundle, false);
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
    let mut joined: Option<String> = None;

    // Proton bare-exe probe (DOOM Eternal case): argv[0] without
    // directories plus the process cwd often reconstructs the install
    // path the DB knows. Only for bare exes (paths with directories
    // already had their full match above); one readlink per miss.
    if found.take_hit().is_none()
      && let Some(exe) = bare_exe(&process_path)
      && let Some(cwd) = read_cwd(process.pid)
    {
      let candidate = format!("{cwd}/{exe}");
      tracing::debug!(
        "[Process Scanner] Bare exe, probing cwd-joined path for pid {}",
        process.pid
      );
      found = self.probe_variants(&candidate, variant_bufs, reversed_path, bundle, false);
      refused |= found.refused();
      if found.take_hit().is_some() {
        found_source = DetectSource::CwdJoined;
        tracing::debug!("[Process Scanner] Cwd match for pid {}", process.pid);
      } else {
        joined = Some(candidate);
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
        let proton_path = joined.as_deref().unwrap_or(&process_path);
        let proton = self.probe_variants(proton_path, variant_bufs, reversed_path, bundle, true);
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
        // loses to a DB-declared path above.
        if let Some(library_appid) =
          self.steam_prefix_app_id_into(lowered, &mut scratch.match_scratch.steam_path)
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
        },
      )
      .is_none(),
      "a path naming nothing declared must stay refused"
    );
  }

  /// The Epic tier's parser: the token is found case-insensitively and
  /// only at an argument boundary, the value lands verbatim, and empty
  /// values or glued tokens are rejected.
  #[test]
  fn epic_id_from_args_finds_the_token() {
    let id = "b0cd075465c44f87be3b505ac04a2e46";
    // Lowercase and mixed-case token, uppercase value (returned verbatim;
    // the caller lowercases for the map lookup).
    assert_eq!(
      epic_id_from_args(Some(&format!("game.exe -epicsandboxid={id}"))),
      Some(id)
    );
    assert_eq!(
      epic_id_from_args(Some("-EpicSandboxId=B0CD075465C44F87BE3B505AC04A2E46")),
      Some("B0CD075465C44F87BE3B505AC04A2E46")
    );
    // Value terminated by a space.
    assert_eq!(
      epic_id_from_args(Some("-epicsandboxid=b0cd -quiet")),
      Some("b0cd")
    );
    // No token at all, and no arguments.
    assert_eq!(epic_id_from_args(Some("-args -here")), None);
    assert_eq!(epic_id_from_args(None), None);
    // Empty value: reject.
    assert_eq!(epic_id_from_args(Some("-epicsandboxid= -verbose")), None);
    // Glued to a preceding word: not a token start.
    assert_eq!(epic_id_from_args(Some("launche-epicsandboxid=b0cd")), None);
  }

  /// One pass finds both Epic tokens; the public wrappers still expose each.
  #[test]
  fn epic_launch_args_reads_both_tokens_in_one_pass() {
    let args = epic_launch_args(Some("game.exe -epicsandboxid=B0CD -epicapp=APP -quiet"));
    assert_eq!(args.sandbox_id, Some("B0CD"));
    assert_eq!(args.app_name, Some("APP"));
    // Order on the command line does not matter.
    let reversed = epic_launch_args(Some("-EpicApp=APP -EpicSandboxId=B0CD"));
    assert_eq!(reversed.sandbox_id, Some("B0CD"));
    assert_eq!(reversed.app_name, Some("APP"));
    assert_eq!(epic_launch_args(None), EpicArgs::default());
  }

  /// The Epic app-name parser: same token rules as the sandbox id, its own
  /// `-epicapp=` token, empty values and glued tokens rejected.
  #[test]
  fn epic_app_from_args_finds_the_token() {
    let app = "8769e24080ea413b8ebca3f1b8c50951";
    assert_eq!(
      epic_app_from_args(Some(&format!("game.exe -epicapp={app}"))),
      Some(app)
    );
    // Token matched case-insensitively; the value lands verbatim.
    assert_eq!(
      epic_app_from_args(Some("-EpicApp=8769E24080EA413B8EBCA3F1B8C50951")),
      Some("8769E24080EA413B8EBCA3F1B8C50951")
    );
    // Value terminated by a space.
    assert_eq!(
      epic_app_from_args(Some("-epicapp=8769e240 -quiet")),
      Some("8769e240")
    );
    // No token at all, and no arguments.
    assert_eq!(epic_app_from_args(Some("-epicsandboxid=b0cd")), None);
    assert_eq!(epic_app_from_args(None), None);
    // Empty value: reject.
    assert_eq!(epic_app_from_args(Some("-epicapp= -verbose")), None);
    // Glued to a preceding word: not a token start.
    assert_eq!(epic_app_from_args(Some("x-epicapp=8769e240")), None);
  }

  /// Epic sandbox id hit: carries its provenance. (Inline: the fixture
  /// needs `SortedIndex::build`, which is crate-private by design.)
  #[test]
  fn match_epic_id_hit_carries_its_source() {
    use crate::bundle::SortedIndex;
    use crate::types::DetectSource;
    use crate::types::{OsName, ScannedExe};

    let entry = Arc::new(ScannedEntry {
      id: "1421192030440263791".into(),
      name: "Grand Theft Auto V Enhanced".into(),
      executables: Box::new([ScannedExe {
        name: "grand theft auto v enhanced/gta5_enhanced.exe".into(),
        os: OsName::Win32,
        is_launcher: false,
        arguments: None,
      }]),
      steam_ids: Box::new([]),
      epic_ids: Box::new(["b0cd075465c44f87be3b505ac04a2e46".into()]),
      aliases: Box::new([]),
    });
    let epic_map = SortedIndex::build(vec![("b0cd075465c44f87be3b505ac04a2e46".into(), 0)]);
    let hit = match_epic_id(
      u64::from(std::process::id()),
      &mut String::new(),
      "b0cd075465c44f87be3b505ac04a2e46",
      // The real Heroic path, normalized: the declared directory is
      // absent, so the bare-basename fallback corroborates.
      "/s:/heroic/gtavenhanced/gta5_enhanced.exe",
      &epic_map,
      &[entry],
      &[],
    )
    .expect("epic id hits");
    assert_eq!(hit.source, DetectSource::EpicAppId);
  }

  /// Epic id without the executable it declares: rejected. Heroic hands the
  /// id to the launcher chain too; corroboration tells the game from it.
  #[test]
  fn match_epic_id_rejects_a_path_that_names_nothing_declared() {
    use crate::bundle::SortedIndex;
    use crate::types::{OsName, ScannedExe};

    let entry = Arc::new(ScannedEntry {
      id: "1421192030440263791".into(),
      name: "Grand Theft Auto V Enhanced".into(),
      executables: Box::new([ScannedExe {
        name: "grand theft auto v enhanced/gta5_enhanced.exe".into(),
        os: OsName::Win32,
        is_launcher: false,
        arguments: None,
      }]),
      steam_ids: Box::new([]),
      epic_ids: Box::new(["b0cd075465c44f87be3b505ac04a2e46".into()]),
      aliases: Box::new([]),
    });
    let epic_map = SortedIndex::build(vec![("b0cd075465c44f87be3b505ac04a2e46".into(), 0)]);
    assert!(
      match_epic_id(
        u64::from(std::process::id()),
        &mut String::new(),
        "b0cd075465c44f87be3b505ac04a2e46",
        "/s:/heroic/gtavenhanced/playgtav.exe",
        &epic_map,
        &[entry],
        &[],
      )
      .is_none(),
      "a path that names none of the entry's executables must be refused"
    );
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
      },
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
        epic_ids: Box::new([]),
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
