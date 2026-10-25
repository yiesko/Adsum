//! Game detection: database projection, matcher bundle, process scanner.
//!
//! The scanner classifies each process against an Aho-Corasick bundle
//! built from Discord's detectable database plus user overrides, and
//! emits deltas. Everything OS-specific lives under [`platform`] behind
//! one uniform surface (the `ProcessEnumerator` seam in `traits`, plus
//! same-signature per-pid readers), so the tick orchestration,
//! the match pipeline, the caches and the event lanes are 100%
//! platform-neutral: `/proc` + netlink on Linux, a `sysinfo` snapshot
//! everywhere else. Reads scale lock-free (`ArcSwap` bundle, `RwLock`
//! exclusions); only genuinely mutable per-tick state sits behind short
//! `Mutex` sections.
//!
//! # Module map
//!
//! - [`matching`] — classification: one process in, an optional hit out
//!   (tiers, path/name rules, command-line evidence, automaton probes).
//! - `sweep` — tick orchestration: enumeration, the full sweep, the
//!   incremental cache tick, slot fills and compaction.
//! - [`scratch`] — the caller-owned reuse buffers both run on (the
//!   zero-allocation contract lives in the type docs).
//! - [`runtime`] — thread lifecycle: the scan loop, refresh loop, event
//!   lanes (snapshot delta + EXEC), cadence and shutdown.
//! - [`platform`] — every OS-specific piece behind one uniform surface
//!   (`ProcessEnumerator` + same-signature readers); `traits` holds the
//!   seam itself.
//! - [`bundle`] — one database generation (automata, arena indexes,
//!   entry list); [`db`] — the wire model, parse and structural caps;
//!   `generations` — the swap/override methods on the server.
//! - [`cache`] — the per-pid memo family (AppId memo + the incremental
//!   classification cache); [`server`] — `ProcessServer` itself;
//!   [`types`] — the slim data model and events; [`refresh`] — the
//!   conditional hourly fetch; `steam`/`heroic` — the store-library
//!   providers' server seams.

pub mod bundle;
pub mod cache;
pub mod db;
mod generations;
mod heroic;
pub mod matching;
pub mod platform;
pub mod refresh;
pub mod runtime;
pub mod scratch;
pub mod server;
mod steam;
mod sweep;
pub(crate) mod traits;
pub mod types;

// The public path stays `adsum_detect::proc_start` (embedders and the
// integration tests use it); the module itself moved under `platform`,
// where its two backends live beside the rest of the OS seams.
pub use platform::proc_start;
pub use types::ProcessDetectedEvent;
