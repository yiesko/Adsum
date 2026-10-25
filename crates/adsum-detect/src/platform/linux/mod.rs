//! The Linux backend: `/proc` readers, the `getdents64` enumerator and
//! the netlink `cn_proc` watcher wiring. Linux is the only target with
//! event delivery, so it is the only per-OS subtree with real mass.

pub(crate) mod enumerator;
pub(crate) mod proc_events;
pub(crate) mod proc_readers;

pub(crate) use enumerator::ProcfsEnumerator;
pub(crate) use proc_readers::{is_suspended, read_cwd_into, read_steam_app_id};
// Test-only conveniences (parity/regression suites reach them through
// the historical `scan::` re-exports).
#[cfg(test)]
pub(crate) use proc_readers::{read_exec, read_exec_into};
