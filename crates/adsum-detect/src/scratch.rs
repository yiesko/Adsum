//! Caller-owned reuse buffers: everything the tick, the platform readers
//! and the match pipeline touch per process WITHOUT allocating.
//!
//! One [`ScanScratch`] set per thread (the scan loop owns one, the EXEC
//! dispatch owns another; one-shot diagnostics pass throwaways). The four
//! groups stay separate types because they have different owners in the
//! pipeline and different lifetimes of borrow — the field-disjointness
//! contract (a shared borrow of one group coexists with an exclusive
//! refill of another) is what keeps the hot path allocation-free without
//! a single big lock of state.

/// The whole per-thread buffer set, grouped so the tick and the match
/// entry point each take one parameter instead of four (and stay under
/// the clippy argument limit).
///
/// The groups stay separate types (different fields, different readers):
/// this grouping is a call-shape convenience, not a merge —
/// [`ExecScratch`] also serves the platform exec reader on its own, and
/// [`EnumerateScratch`] the enumerator.
#[derive(Default)]
pub struct ScanScratch {
  /// Match-path buffers (name/folder normalization, Steam path, stat,
  /// cwd probe).
  pub match_scratch: MatchScratch,
  /// Per-pid read buffers (cmdline, args, environ, `/proc` path) plus
  /// the slot-compaction counter.
  pub exec_scratch: ExecScratch,
  /// Automaton-probe buffers (path variants + reversed probe string).
  /// Disjoint from every other group, so the match path can hold a
  /// shared borrow of `match_scratch.lowered`/`cwd_join` while refilling
  /// these.
  pub probe: ProbeBufs,
  /// Enumeration-pass state: the tick's pid list and the incremental
  /// mode's sweep/work lists. Disjoint from the reader buffers, so the
  /// sweep can iterate `enumerate.pids` while the reader refills
  /// `exec_scratch` per pid.
  pub enumerate: EnumerateScratch,
}

/// One enumeration pass's reuse state (see
/// `ProcessEnumerator` seam): the pid list
/// every backend fills, plus the sets and work lists the per-tick sweeps
/// and the incremental mode derive from it. All cleared-and-refilled per
/// tick, so a warm tick allocates nothing here.
#[derive(Default)]
pub struct EnumerateScratch {
  /// Reused pid list of one enumeration pass (`begin_tick` fills it on
  /// every backend, in enumeration order — the order the detected table
  /// reports in).
  pub(crate) pids: Vec<u64>,
  /// Reused `getdents64` arena (Linux), allocated as `u64` cells so the
  /// base pointer carries the 8-byte alignment the syscall's records
  /// want. Sized once on first use (`DENT_ARENA_U64S`), never grown
  /// again: `/proc` directory entries are tiny, and a short batch simply
  /// means one more `getdents64` call, never a realloc.
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  pub(crate) dents: Vec<u64>,
  /// Reused liveness set for the per-tick dead-entry sweeps (AppId memo
  /// and process cache): one set build per tick was a fresh allocation
  /// per tick; the sweeps now clear and refill this one instead.
  pub(crate) live_pids: rustc_hash::FxHashSet<u64>,
  /// Reused work list of the incremental tick: pids whose cache entry is
  /// missing or foreign-epoch, collected under the cache lock and
  /// drained by the reader loop. Cleared and refilled, never rebuilt.
  pub(crate) work_pids: Vec<u64>,
  /// Reused list of cached-HIT pids the incremental tick revalidates
  /// (one read each, 0-3 per tick): a zombied game is still enumerated
  /// (it has a `/proc` dir; sysinfo lists it too) but unreadable, and
  /// the full sweep's "unreadable = absent" rule must hold for cached
  /// hits too.
  pub(crate) hit_pids: Vec<u64>,
}

/// Reusable intermediates of the per-pid readers
/// (`read_exec_into` and friends):
/// one set per thread, owned by the calling loop (scan thread, EXEC
/// dispatch). The path buffer, cmdline read buffer and args join buffer
/// are allocated once and cleared per pid, replacing the
/// per-process-per-tick temporaries (previously ~400 allocations every
/// 5s). The read buffers are only touched by the Linux `/proc` readers —
/// the snapshot backend copies straight into the slot — but the type is
/// part of every target's API (and carries the slot-compaction counter,
/// which both fills share).
#[derive(Default)]
pub struct ExecScratch {
  /// `/proc/<pid>/…` path buffer (Linux readers; the snapshot backend
  /// resolves pids through the sysinfo table and never spells paths).
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  pub(crate) path: String,
  /// `/proc/<pid>/cmdline` body (Linux reader).
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  pub(crate) cmdline: Vec<u8>,
  /// argv-join buffer trading places with the slot's `arguments`
  /// (Linux reader; the snapshot backend joins straight into the slot).
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  pub(crate) args: String,
  /// `/proc/<pid>/environ` body for the AppId lookup: the only
  /// multi-kilobyte read of the scan. Reused across pids so the buffer
  /// keeps its capacity instead of regrowing per newly-seen process.
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  pub(crate) environ: Vec<u8>,
  /// Consecutive ticks where the live process count sat well under the
  /// slot count, so a single transient spike does not pin the extra
  /// slots (and their string buffers) for the rest of the session.
  pub(crate) low_ticks: u8,
}

/// Reusable buffers for the match path: `lowered` holds the lowercased
/// process path per miss, `norm_out` the normalized folder per
/// component. One set per thread, owned by the calling loop (scan loop,
/// EXEC dispatch) — cleared and refilled, never rebuilt per process.
/// Borrowing contract: `lowered` and `norm_out` are disjoint fields, so
/// a shared `lowered` borrow coexists with an exclusive `norm_out`
/// refill across the folder loops.
#[derive(Default)]
pub struct MatchScratch {
  pub lowered: String,
  pub norm_out: String,
  /// Canonical Steam path for the install-dir evidence (lowercase, `/`
  /// separators, leading `/`): reused across processes so a miss costs
  /// no allocation. Touched only on the AppId corroboration and the
  /// Steam-library tier, never on the hot path.
  pub steam_path: String,
  /// `/proc/<pid>/stat` body for the suspended check: tiny, but read for
  /// every aux hit (0-3 per tick), so it pays to reuse the buffer rather
  /// than build a fresh `String` (plus the path) per hit. Also carries
  /// the `/proc/<pid>/stat` path itself before the body (see
  /// `is_suspended`). Cleared before
  /// every read so a failed one can never leave a stale line to parse.
  pub stat: String,
  /// `/proc/<pid>/cwd` readlink target for the bare-exe probe (see
  /// `read_cwd_into`): raw bytes,
  /// lowercased in place, reused across processes so a miss costs no
  /// allocation.
  pub cwd: Vec<u8>,
  /// The cwd-joined candidate path (`<cwd>/<exe>`) for the bare-exe
  /// probe: reused across processes like every other buffer here (the
  /// old `format!` allocated one `String` per bare-exe miss, the common
  /// shape under Proton).
  pub cwd_join: String,
  /// `/proc/<pid>/…` path buffer for the lazy link reads
  /// (`read_cwd_into`): reused, NUL
  /// terminated in place for the raw syscall.
  pub proc_path: String,
}

/// Reusable automaton-probe buffers: the 64-bit-marker path variants and
/// the reversed string the automatons walk. One set per thread, cleared
/// and refilled per process per probe — they used to be separate
/// function parameters threaded beside the scratch; living inside
/// [`ScanScratch`] keeps the match entry point under the clippy argument
/// limit with the exclusions guard added.
#[derive(Default)]
pub struct ProbeBufs {
  pub variants: [String; 5],
  pub reversed: String,
}

/// Consecutive low-occupancy ticks before the slot list is compacted.
///
/// Long enough that a momentary spike never triggers a realloc, short
/// enough that a sustained drop (build finished, game exited) releases
/// the surplus within ~40 s at the base cadence.
///
/// Shared by both platforms' slot fills (`compact_slots`): the sustained
/// drop rule protects the sysinfo snapshot's slots exactly like the
/// `/proc` sweep's.
pub(crate) const SLOT_LOW_TICKS: u8 = 8;
