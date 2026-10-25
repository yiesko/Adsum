//! Scanner data model: slim match inputs, hits and scan state.
//!
//! The slim forms (`ScannedEntry`, `ScannedExe`) carry only what matchers
//! read: storing full [`DetectableActivity`]s
//! would pin ~3x the memory per database generation.

use std::sync::{Arc, Mutex};

use adsum_types::AppId;

use crate::db::DetectableActivity;

#[derive(Default, Clone)]
pub struct ProcessScanState {
  pub obs_open: bool,
}

#[derive(Default)]
pub struct ProcessEventListeners {
  /// Optional scan-tick callback, invoked with the post-tick state.
  pub on_process_scan_complete: Option<Arc<Mutex<ProcessCallback>>>,
}

#[derive(Clone, Default)]
pub struct Exec {
  pub pid: u64,
  pub path: String,
  pub arguments: Option<String>,
}

/// Scanner event: one game appeared, the whole table went empty, or one
/// previously-announced slot vanished while others remain.
///
/// Enum (not `Option`) because the states are mutually exclusive: a
/// per-slot clear must not be mistaken for a full-table clear downstream.
#[derive(Clone, Debug)]
pub enum ProcessDetectedEvent {
  /// One classified game (re-emits are deduped downstream).
  Detected(ScannedHit),
  /// No games detected: clear every outstanding generic publication.
  Cleared,
  /// A slot present in the previous non-empty snapshot but absent now.
  /// Carries the last-known app id + pid so the bridge can clear exactly
  /// that card without flapping co-running games.
  Removed {
    /// Application id of the vanished slot. `AppId` (an `Arc<str>`
    /// newtype): the snapshot lane already holds the id refcounted, so
    /// the event clones a refcount instead of copying the bytes (the
    /// old `Box<str>` copied per removal), and the bridge's
    /// `ProcInput::Removed(AppId, _)` adopts it without conversion.
    id: AppId,
    /// Last-known pid (for the empty payload + logs).
    pid: u64,
  },
}

impl ProcessDetectedEvent {
  /// One classified game.
  #[must_use]
  pub fn detected(hit: ScannedHit) -> Self {
    Self::Detected(hit)
  }

  /// The empty-table clear event (the old `id == "null"` convention, now
  /// explicit).
  #[must_use]
  pub fn cleared() -> Self {
    Self::Cleared
  }

  /// A per-slot clear for a vanished snapshot entry.
  #[must_use]
  pub fn removed(id: AppId, pid: u64) -> Self {
    Self::Removed { id, pid }
  }
}

/// One executable as the scanner needs it: matcher inputs only. Slimmer
/// than [`DetectableActivity`]'s full form — same field names, so match
/// code reads unchanged. Text rides as `Box<str>` (16B, no capacity
/// field) instead of `String` (24B): ~11k executables × 2 string fields
/// is real money, and these never grow after the build.
///
/// `Exec.arguments` (the scan tick's per-process slot) stays a `String`:
/// `read_exec_into` recycles that buffer with `mem::take`, which a
/// `Box<str>` cannot take part in. Only this build-once field boxes.
#[derive(Clone, Debug)]
pub struct ScannedExe {
  pub name: Box<str>,
  pub os: OsName,
  pub is_launcher: bool,
  pub arguments: Option<Box<str>>,
}

/// Executable OS: the database only ever carries three values (`win32`
/// × 11k, `linux`/`darwin` handful), so hot values are unit variants
/// (zero allocation) and anything exotic rides generation-owned inside
/// `Other` — freed with the bundle on the next swap. This replaced a
/// `&'static str` interning that leaked every unknown value for the
/// process lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsName {
  Win32,
  Linux,
  Darwin,
  Empty,
  Other(Box<str>),
}

impl OsName {
  /// Classify a database `os` tag, cloning only exotic values.
  pub fn from_os(os: &str) -> Self {
    match os {
      "win32" => Self::Win32,
      "linux" => Self::Linux,
      "darwin" => Self::Darwin,
      "" => Self::Empty,
      other => Self::Other(other.to_owned().into_boxed_str()),
    }
  }

  /// Zero-copy twin of [`from_os`](OsName::from_os) for owned inputs:
  /// exotic values move instead of cloning.
  pub fn from_string(os: String) -> Self {
    match os.as_str() {
      "win32" => Self::Win32,
      "linux" => Self::Linux,
      "darwin" => Self::Darwin,
      "" => Self::Empty,
      _ => Self::Other(os.into_boxed_str()),
    }
  }

  /// Back to the database tag string (borrows exotic values in place).
  pub fn as_str(&self) -> &str {
    match self {
      Self::Win32 => "win32",
      Self::Linux => "linux",
      Self::Darwin => "darwin",
      Self::Empty => "",
      Self::Other(os) => os,
    }
  }

  /// True for the empty tag (database entries without an OS).
  pub fn is_empty(&self) -> bool {
    matches!(self, Self::Empty)
  }
}

/// One database game as the scanner needs it: matching identity plus
/// publish labels, nothing else. A full [`DetectableActivity`] carries
/// ~30 mostly-`None` fields (`Option<String>` costs 24B even when empty),
/// so storing 24k of them pins ~13MB; the slim form keeps under a third
/// of that. Converted once per generation at build time from the full
/// public struct (which stays the API and transport shape) — only
/// Steam and Epic distributor SKUs survive the trip, since only those
/// matchers read them.
#[derive(Clone, Debug)]
pub struct ScannedEntry {
  /// Refcounted like the bridge's `ScannedGame.id`: the detection
  /// boundary (`daemon.rs` pump) clones a refcount instead of copying
  /// the bytes out of a `Box<str>` per detection, and the snapshot lane
  /// reuses the same allocation per tick (see `AppId`).
  pub id: AppId,
  /// Refcounted for the same reason (`ScannedGame.name` adopts it by
  /// clone). Both fields stay 16 bytes: `Arc<str>` and `Box<str>` are
  /// both fat pointers, so the layout assert below is unchanged.
  pub name: Arc<str>,
  /// Boxed slices, not `Vec`: the generation is built once and then never
  /// grows, shrinks or mutates these. `Box<[T]>` drops the excess capacity
  /// a `Vec` may carry and shrinks each field by a pointer on 64-bit,
  /// which matters across ~24k entries.
  pub executables: Box<[ScannedExe]>,
  pub steam_ids: Box<[Box<str>]>,
  /// Epic sandbox ids (`epic` distributor SKUs), lowercased: the database
  /// stores them uppercased, the command line carries them lowercased.
  pub epic_ids: Box<[Box<str>]>,
  pub aliases: Box<[Box<str>]>,
}

impl ScannedEntry {
  /// Convert a full public entry to the slim scanner form (borrowing):
  /// used where the input is shared (`Arc` main list). See
  /// [`from_owned`](ScannedEntry::from_owned) for owned batches.
  pub fn from_activity(activity: &DetectableActivity) -> Self {
    Self {
      id: AppId::from(activity.id.clone()),
      name: Arc::from(activity.name.as_str()),
      executables: activity
        .executables
        .as_ref()
        .map(|exes| {
          exes
            .iter()
            .map(|exe| ScannedExe {
              name: exe.name.clone().into_boxed_str(),
              os: OsName::from_os(&exe.os),
              is_launcher: exe.is_launcher,
              arguments: exe.arguments.as_deref().map(Box::<str>::from),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
        })
        .unwrap_or_default(),
      steam_ids: activity
        .third_party_skus
        .as_ref()
        .map(|skus| {
          skus
            .iter()
            .filter(|sku| sku.distributor == "steam")
            .filter_map(|sku| sku.id.clone())
            .filter(|id| !id.is_empty())
            .map(|id| id.into_boxed_str())
            .collect::<Vec<_>>()
            .into_boxed_slice()
        })
        .unwrap_or_default(),
      epic_ids: activity
        .third_party_skus
        .as_ref()
        .map(|skus| {
          skus
            .iter()
            .filter(|sku| sku.distributor == "epic")
            .filter_map(|sku| sku.id.as_deref())
            .filter(|id| !id.is_empty())
            .map(|id| id.to_ascii_lowercase().into_boxed_str())
            .collect::<Vec<_>>()
            .into_boxed_slice()
        })
        .unwrap_or_default(),
      aliases: activity
        .aliases
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|alias| alias.into_boxed_str())
        .collect::<Vec<_>>()
        .into_boxed_slice(),
    }
  }

  /// Same conversion for an owned entry: moves strings instead of cloning
  /// them, so batch conversions (hourly refresh, override appends) skip
  /// ~130k duplicate allocations. Same output as [`from_activity`](ScannedEntry::from_activity).
  pub fn from_owned(activity: DetectableActivity) -> Self {
    // One sweep over the SKUs moves each id string exactly once: the
    // steam field keeps its move, the epic ids lowercase theirs.
    let mut steam: Vec<Box<str>> = Vec::new();
    let mut epic: Vec<Box<str>> = Vec::new();
    for sku in activity.third_party_skus.unwrap_or_default() {
      let Some(id) = sku.id.filter(|id| !id.is_empty()) else {
        continue;
      };
      match sku.distributor.as_str() {
        "steam" => steam.push(id.into_boxed_str()),
        // Lowercased here: the database stores `B0CD…`, the command line
        // carries `b0cd…`.
        "epic" => epic.push(id.to_ascii_lowercase().into_boxed_str()),
        _ => {}
      }
    }
    Self {
      // `Arc<str>` from `String` copies into the refcounted layout (one
      // small copy per entry per generation, build-time only); every
      // later hop — hit, event, bridge — is a refcount bump.
      id: AppId::from(activity.id),
      name: Arc::from(activity.name.as_str()),
      executables: activity
        .executables
        .unwrap_or_default()
        .into_iter()
        .map(|exe| ScannedExe {
          name: exe.name.into_boxed_str(),
          os: OsName::from_string(exe.os),
          is_launcher: exe.is_launcher,
          arguments: exe.arguments.map(String::into_boxed_str),
        })
        .collect::<Vec<_>>()
        .into_boxed_slice(),
      steam_ids: steam.into_boxed_slice(),
      epic_ids: epic.into_boxed_slice(),
      aliases: activity
        .aliases
        .unwrap_or_default()
        .into_iter()
        .map(|alias| alias.into_boxed_str())
        .collect::<Vec<_>>()
        .into_boxed_slice(),
    }
  }
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

/// Which matcher classified the process: the provenance asked for when
/// a detection needs explaining.
///
/// Re-exported from `adsum-types` so every existing `crate::types::`
/// import and `pub use` keeps resolving. It lives there because the value
/// crosses the scanner/bridge boundary inside `ScannedGame`, and the
/// bridge deliberately does not depend on the detector — see
/// [`adsum_types::DetectSource`] for the rationale.
pub use adsum_types::DetectSource;

/// One classified game: the shared slim entry plus the observation (pid +
/// epoch-millis start). Replaces stamping pid/timestamp onto a full-struct
/// clone per hit per tick — zero per-hit allocation beyond the timestamp
/// read itself.
#[derive(Clone, Debug)]
pub struct ScannedHit {
  pub entry: Arc<ScannedEntry>,
  pub pid: u64,
  pub start: u64,
  /// Which matcher classified the process (detection provenance).
  pub source: DetectSource,
}

impl ScannedHit {
  /// Attach the observation (pid + now as epoch millis) to a shared
  /// entry: no clone, unlike the old full-struct stamp.
  pub fn stamp(entry: Arc<ScannedEntry>, pid: u64, source: DetectSource) -> Self {
    // Epoch millis as a NUMBER: Discord's schema (and strict clients)
    // want an integer here — a stringified timestamp is silently
    // dropped downstream.
    Self {
      entry,
      pid,
      start: epoch_ms(),
      source,
    }
  }
}

/// Now as epoch millis (0 when the clock is unreadable): single
/// canonical clock for stamps and latency math.
#[must_use]
pub fn epoch_ms() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .ok()
    .and_then(|age| u64::try_from(age.as_millis()).ok())
    .unwrap_or(0)
}

// 3 x Vec (24 B) became 3 x Box<[T]> (16 B): 24 B off per entry. `epic_ids`
// joined later as a fourth boxed slice (data model, another 16 B per entry).
const _: () = assert!(std::mem::size_of::<ScannedEntry>() <= 96);

// `Option<String>` (24 B) became `Option<Box<str>>` (16 B).
const _: () = assert!(std::mem::size_of::<ScannedExe>() <= 64);

/// Callback invoked with the scan state after every scan tick (e.g. the
/// OBS-open flag for streamer-mode tooling).
pub type ProcessCallback = dyn FnMut(ProcessScanState) + Send + Sync;
