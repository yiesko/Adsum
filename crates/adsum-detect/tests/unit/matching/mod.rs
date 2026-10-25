use super::*;
// The buffer types moved to `crate::scratch` (out of the old scan.rs);
// the leaf suites reach them through this hub's glob like everything
// else. Three of them serve only the Linux-gated leaves below, so they
// ride the same gate; `MatchScratch` is used by the ungated leaves too.
use crate::scratch::MatchScratch;
#[cfg(target_os = "linux")]
use crate::scratch::{ExecScratch, SLOT_LOW_TICKS, ScanScratch};

mod epic;
mod matching;
mod name_matching;

// Match-path allocation reuse: the into-variants refill caller-owned
// buffers with byte-identical outputs (corpus below covers case,
// punctuation, dots, whitespace, empties and non-ASCII).
mod normalization;

// Per-pid allocation reuse for the scan tick: `read_exec_into` refills
// caller-owned buffers and `process_list_into` reuses every `Exec` slot
// across ticks. Parity + no-realloc regression tests. Both helpers are
// Linux-only (`/proc`), so the module is too.
#[cfg(all(test, target_os = "linux"))]
mod reuse;

// `/proc`-only: `process_list_into` is the Linux sweep and the slot
// release it drives is measured through it, so the module is gated the
// same way as `reuse` above.
#[cfg(all(test, target_os = "linux"))]
mod slot_compaction;
mod steam;
