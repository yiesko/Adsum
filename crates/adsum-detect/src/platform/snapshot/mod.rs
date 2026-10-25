//! The snapshot backend: every non-Linux target, one implementation.
//!
//! `sysinfo`'s API is OS-agnostic, so Windows and macOS share this
//! subtree instead of each getting a near-empty one; the genuinely
//! per-OS fragments are elsewhere by design — arena release in
//! `platform::memory`, Steam/Heroic discovery in `adsum-steam`/`heroic`
//! (registry + `%APPDATA%` on Windows, `~/Library/Application Support`
//! on macOS).

pub(crate) mod enumerator;
pub(crate) mod readers;

pub(crate) use enumerator::SnapshotEnumerator;
pub(crate) use readers::{is_suspended, read_cwd_into, read_steam_app_id};
