//! Windows-specific helpers: none today, and that is the finding.
//!
//! Every Windows-only concern this crate has is already unified behind
//! a shared seam: enumeration and per-pid reads go through
//! `platform::snapshot` (sysinfo), the environ-backed SteamAppId reader
//! lives there too (PEB via the process handle), arena release is
//! `platform::memory`'s `HeapCompact` arm, and Steam-root discovery
//! (registry `SteamPath` + `Program Files`) lives in `adsum-steam`
//! beside its Linux and macOS shapes. This module is the documented
//! home for anything genuinely Windows-only that appears later (an ETW
//! watcher would land here, mirroring `platform::linux::proc_events`).
