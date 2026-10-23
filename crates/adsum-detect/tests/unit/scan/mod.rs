use super::*;

mod epic;
mod first_sightings;
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
