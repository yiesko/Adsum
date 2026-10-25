//! The per-pid memo family: everything the scanner remembers about one
//! process between ticks, and the rules that keep those memories honest.
//!
//! Two members, one trust model — each is keyed by pid, swept against
//! the enumerated live set every tick, and invalidated by the signals
//! that can change its answer:
//!
//! - `appid` — the SteamAppId memo (`AppIdMemo`): environ is
//!   kilobytes and never changes after exec, so one read per process
//!   lifetime; `EXEC` invalidates through a sequence number that
//!   discards racing stale reads.
//! - `procs` — the incremental scan's classification cache
//!   (`CachedProc`): one match-pipeline result per pid per bundle
//!   generation, with every watcher loss channel mapped to a flush (see
//!   the module docs for the full invalidation matrix).
//!
//! Grouped because they fail together: a watcher that loses events makes
//! BOTH memories untrustworthy, and the tick that flushes one sweeps the
//! other against the same liveness set.

pub(crate) mod appid;
pub(crate) mod procs;

pub(crate) use appid::AppIdMemo;
pub(crate) use procs::ProcCache;
