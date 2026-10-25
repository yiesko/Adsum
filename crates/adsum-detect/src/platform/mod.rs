//! Platform backends: every OS-specific piece of the scanner lives
//! under this module, and everything shared consumes the uniform
//! surface it re-exports. The rest of the crate (bundle, scan,
//! sweep, runtime, cache, ...) is 100% platform-neutral.
//!
//! The backend contract — each platform provides the same items under
//! the same names:
//! - `PlatformEnumerator`: this target's `ProcessEnumerator`
//!   (crate::traits) implementation (`ProcfsEnumerator` on Linux, `SnapshotEnumerator`
//!   everywhere else);
//! - the lazy per-pid readers with identical signatures:
//!   `read_exec_into`, `read_steam_app_id`, `read_cwd_into`,
//!   `is_suspended` — the match pipeline calls them without knowing
//!   which backend answers;
//! - `memory::release_parse_arenas` (four OS backends, one entry point)
//!   and `proc_start::process_start_ms` (two backends, one public path).
//!
//! Module map — why some branches of the tree are thin:
//! - `linux/` is the only per-OS subtree with real mass: the `/proc`
//!   readers, the raw `getdents64` enumerator and the netlink watcher
//!   wiring (EXEC/EXIT dispatch threads).
//! - `snapshot/` serves EVERY non-Linux target: the backend is
//!   `sysinfo`, whose API is OS-agnostic, so there is no per-OS
//!   enumeration code left to house. Named for the mechanism instead of
//!   `sysinfo` so in-module paths never shadow the external crate.
//! - `macos/` carries Apple filesystem *shape* knowledge (the `.app`
//!   bundle interior). Deliberately NOT cfg-gated: that match tier is
//!   data-driven — the database ships `darwin` executables on every
//!   platform and a custom override may declare `.app` anywhere — so it
//!   must compile, run and be tested everywhere.
//! - `windows/` is empty because nothing Windows-only is left in this
//!   crate: arena release is unified in `memory.rs`, Steam/Heroic
//!   discovery lives in `adsum-steam`/`heroic` (registry + `%APPDATA%`
//!   included), and enumeration/readers go through `snapshot/`. It is
//!   the documented home for any that appear.

pub(crate) mod memory;
pub mod proc_start;

pub(crate) mod macos;
pub(crate) mod windows;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(not(target_os = "linux"))]
pub(crate) mod snapshot;

// The uniform reader/enumerator surface the shared code imports. Linux
// exposes the raw /proc readers (plus `parse_stat_state`, which only
// Linux stat lines can feed); the snapshot backend exposes the same
// signatures over `sysinfo` (environ AppIds included — the stubs these
// replaced are gone).
#[cfg(target_os = "linux")]
pub(crate) use linux::{
  ProcfsEnumerator as PlatformEnumerator, is_suspended, read_cwd_into, read_steam_app_id,
};
// Test-only conveniences (the in-crate Linux suites reach them through
// the historical `scan::` re-exports): the whole-`Exec` wrapper and the
// slot reader.
#[cfg(all(target_os = "linux", test))]
pub(crate) use linux::{read_exec, read_exec_into};
#[cfg(not(target_os = "linux"))]
pub(crate) use snapshot::{
  SnapshotEnumerator as PlatformEnumerator, is_suspended, read_cwd_into, read_steam_app_id,
};
