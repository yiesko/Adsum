//! IPC-wins handoff: generic process detection yields its slot to a live
//! game-SDK presence and reclaims it when that source clears.
//!
//! Retro-compat rule (several companions may target one app): **last
//! publisher wins**. Each app maps to the pid of its current owner; a
//! publish replaces the owner, and a clear only releases the slot when it
//! comes from that same owner.

use std::collections::HashMap;
use std::sync::Arc;

use adsum_types::{AppId, DetectSource};

/// One process-detected game, remembered so a clear can hand the slot back
/// to generic detection (the scanner only emits on *changes*).
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedGame {
  /// Discord application id of the detected game.
  pub id: AppId,
  /// Human-readable game name for logs and generic cards. Refcounted
  /// like the sibling `id`: the resume/clear chain clones it per hop, and
  /// a deep `String` copy there was ~3 of the 4 allocations per detection.
  pub name: Arc<str>,
  /// OS pid of the detected process.
  pub pid: u64,
  /// Process start time (Unix seconds) for card timestamps.
  pub start: u64,
  /// Which matcher classified the process: travels to generic payloads
  /// and snapshots for detection provenance. `Copy`, so a hop is free.
  pub source: DetectSource,
  /// Process start as epoch millis, when the detector could read it:
  /// snapshot latency is measured against this, never re-read.
  pub process_start_ms: Option<u64>,
}

/// Scanner input to the bridge: one game appeared, one slot vanished, or
/// the table is empty.
#[derive(Clone, Debug)]
pub enum ProcInput {
  /// A game was detected (re-emits are deduped downstream).
  Detected(ScannedGame),
  /// A `(app id, pid)` pair present in the previous snapshot but absent
  /// now, while others remain: clear exactly this card without flapping
  /// co-running games. The pid gates the removal (see `note_remove`): a
  /// stale removal must never clear a newer detection of the same slot
  /// (pid reuse, EXEC-vs-poll race).
  Removed(AppId, u64),
  /// No games detected: clear every outstanding generic publication.
  Cleared,
}

/// Cap for the handoff tables: distinct live app-ids are tiny in practice
/// (co-running games plus their companions). The cap only bites a client
/// publishing hundreds of ids without clearing (malicious or buggy) —
/// without it, memory grows forever on untrusted input. Enforcement
/// purges dead owners first (the actual garbage), so live slots are only
/// evicted in pathological cases, and even then the next scan or publish
/// re-arms them (self-healing).
pub const MAX_HANDOFF_ENTRIES: usize = 64;

/// Fallible `u64` pid narrowing for `libc::kill`: kernel pids fit `pid_t`
/// (`i32` on 64-bit unix), so anything wider names no process. Same gate
/// as its only caller (the non-Linux probe below); `libc` is a `cfg(unix)`
/// dependency of this crate.
#[cfg(all(unix, not(target_os = "linux")))]
fn pid_to_pid_t(pid: u64) -> Option<libc::pid_t> {
  libc::pid_t::try_from(pid).ok()
}

/// Best-effort liveness probe so a clear for an already-dead game doesn't
/// flash the generic card on the way out (the scanner's null event clears
/// the slot anyway).
#[must_use]
pub fn is_process_alive(pid: u64) -> bool {
  if pid == 0 {
    return false;
  }
  #[cfg(target_os = "linux")]
  {
    use std::os::unix::ffi::OsStrExt;
    // The pid renders into a stack buffer ("/proc/" + ≤20 digits fits in
    // 32): the probe can fire several times per clear/resume sequence,
    // and the old `format!` was its only allocation.
    let mut buf = [0u8; 32];
    let mut end = buf.len();
    let mut rest = pid;
    loop {
      end -= 1;
      buf[end] = b'0' + (rest % 10) as u8;
      rest /= 10;
      if rest == 0 {
        break;
      }
    }
    end -= 6;
    buf[end..end + 6].copy_from_slice(b"/proc/");
    std::path::Path::new(std::ffi::OsStr::from_bytes(&buf[end..])).exists()
  }
  #[cfg(all(unix, not(target_os = "linux")))]
  {
    // SAFETY: signal 0 performs no action; only error reporting. A zero
    // return (or EPERM: exists but unowned) means alive; ESRCH means dead.
    match pid_to_pid_t(pid) {
      Some(narrow) => {
        (unsafe { libc::kill(narrow, 0) }) == 0
          || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
      }
      // Beyond `pid_t` range (e.g. `u32::MAX` where `pid_t` is `i32`): no
      // such process can exist, and a truncating `as` cast would alias it
      // onto a live id (notably `-1`, the whole process group).
      None => false,
    }
  }
  #[cfg(windows)]
  {
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // Pids beyond u32 cannot exist: narrowing is safe by construction.
    let Ok(pid_u32) = u32::try_from(pid) else {
      return false;
    };
    // SAFETY: a query-only handle has no side effects on the target and
    // is always closed before returning; null means no such process.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid_u32) };
    if handle.is_null() {
      return false;
    }
    unsafe { CloseHandle(handle) };
    true
  }
  #[cfg(not(any(unix, windows)))]
  {
    // Platforms without a probe: assume alive (ghost reaping stays off
    // rather than risking live cards).
    true
  }
}

/// Why a visible card was (or is being) cleared: the attribution asked
/// for when a presence disappears. `Copy` and allocation-free — it only
/// rides log lines and snapshot fields, never hot structures.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClearReason {
  /// Genuine null-activity CLEAR frame from the owning connection.
  SdkClear,
  /// Owning socket died without CLEAR (ghost reap, pid-owned release).
  AbruptClose,
  /// Scanner reports the process gone and liveness confirms death.
  ProcessVanished,
  /// Process alive but no longer classified (database refresh or
  /// ignore-list dropped the match while the pid still runs).
  ScanAbsent,
  /// Generic card withdrawn for a live SDK owner on the same slot.
  Yielded,
}

impl ClearReason {
  /// Stable log identifier for journal correlation.
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::SdkClear => "sdk-clear",
      Self::AbruptClose => "abrupt-close",
      Self::ProcessVanished => "process-vanished",
      Self::ScanAbsent => "scan-absent",
      Self::Yielded => "yielded",
    }
  }
}

/// IPC-wins handoff state. One lock for the whole state (short critical
/// sections, no I/O under it), shared by the event and process pumps.
#[derive(Clone, Debug, Default)]
pub struct HandoffState {
  /// App id → pid of its current IPC/WS owner (`SET_ACTIVITY` with activity).
  live_ipc: HashMap<AppId, u64>,
  /// Last games the scanner reported, by app id (a null/clear event wipes
  /// the table).
  last_scans: HashMap<AppId, ScannedGame>,
}

impl HandoffState {
  /// Record a live SDK publication for `app_id` from `pid`.
  pub fn note_publish(&mut self, app_id: &str, pid: u64) {
    if self.live_ipc.len() >= MAX_HANDOFF_ENTRIES {
      // Purge dead owners first (the actual garbage: crashed companions
      // that never cleared). Whatever remains is live.
      self.live_ipc.retain(|_, owner| is_process_alive(*owner));
    }
    self.live_ipc.insert(AppId::from(app_id), pid);
    // Hard bound: even all-live flooding (one pid, infinite ids) stops
    // here. Evicting a live slot only desuppresses its generic until the
    // next publish re-arms it — unreachable in legitimate use (<5 ids).
    while self.live_ipc.len() > MAX_HANDOFF_ENTRIES {
      let Some(victim) = self.live_ipc.keys().next().cloned() else {
        break;
      };
      self.live_ipc.remove(&victim);
    }
  }

  /// Record a clear; returns true when the slot was actually released
  /// (the clear came from the owning pid).
  pub fn note_clear(&mut self, app_id: &str, pid: u64) -> bool {
    if self.live_ipc.get(app_id).is_some_and(|owner| *owner == pid) {
      self.live_ipc.remove(app_id);
      true
    } else {
      false
    }
  }

  /// Forget one scanner slot (per-slot clear while others remain).
  /// Removes only when the stored pid matches the removal event: a stale
  /// removal (pid reuse, EXEC-vs-poll race) must not drop a newer scan.
  /// Returns the game previously remembered there, if released.
  pub fn note_remove(&mut self, app_id: &str, pid: u64) -> Option<ScannedGame> {
    if self
      .last_scans
      .get(app_id)
      .is_some_and(|known| known.pid == pid)
    {
      self.last_scans.remove(app_id)
    } else {
      None
    }
  }

  /// Record a scanner report (`None` = table empty, forget every game).
  pub fn note_scan(&mut self, game: Option<ScannedGame>) {
    match game {
      Some(game) => {
        if self.last_scans.len() >= MAX_HANDOFF_ENTRIES {
          self
            .last_scans
            .retain(|_, known| is_process_alive(known.pid));
        }
        self.last_scans.insert(game.id.clone(), game);
        while self.last_scans.len() > MAX_HANDOFF_ENTRIES {
          let Some(victim) = self.last_scans.keys().next().cloned() else {
            break;
          };
          self.last_scans.remove(&victim);
        }
      }
      None => self.last_scans.clear(),
    }
  }

  /// Release every slot owned by `pid` (abrupt close without CLEAR) and
  /// return the released app ids. Without this, a dead owner suppresses
  /// its slots' generics forever.
  pub fn note_clear_pid(&mut self, pid: u64) -> Vec<AppId> {
    self
      .live_ipc
      .extract_if(|_, owner| *owner == pid)
      .map(|(app, _)| app)
      .collect()
  }

  /// Whether generic detection must stay out of this slot right now.
  #[must_use]
  pub fn is_suppressed(&self, app_id: &str) -> bool {
    self.live_ipc.contains_key(app_id)
  }

  /// Pid currently owning `app_id`'s slot, if a live SDK source holds it
  /// (for takeover log lines naming both sides).
  #[must_use]
  pub fn owner_of(&self, app_id: &str) -> Option<u64> {
    self.live_ipc.get(app_id).copied()
  }

  /// The game to re-assert when `app_id`'s IPC source cleared, if the
  /// scanner still reports that same game.
  #[must_use]
  pub fn resume_for(&self, app_id: &str) -> Option<ScannedGame> {
    self.last_scans.get(app_id).cloned()
  }
}

/// Track a generic publication for its later clear, bounded like the
/// handoff tables above (purge dead pids first, then evict arbitrarily).
/// Evicting a live entry only drops its future clear — the next scan
/// re-arms it (self-healing).
pub fn track_process_publication(map: &mut HashMap<AppId, u64>, app_id: AppId, pid: u64) {
  if map.len() >= MAX_HANDOFF_ENTRIES {
    map.retain(|_, known| is_process_alive(*known));
  }
  map.insert(app_id, pid);
  while map.len() > MAX_HANDOFF_ENTRIES {
    let Some(victim) = map.keys().next().cloned() else {
      break;
    };
    map.remove(&victim);
  }
}

#[cfg(test)]
#[path = "../tests/unit/handoff.rs"]
mod tests;
