//! Allocator hygiene, unified: one function, four platform backends.
//!
//! One-time parse arenas (fetch body, JSON DOM, trimmed copy, automaton
//! build scratch) are garbage the moment the structures they built are
//! live; without returning them, the allocator holds the startup/refresh
//! spike as RSS indefinitely. Callers are the initial bundle build, every
//! hourly rebuild and every custom-override rebuild — the cadence itself
//! is owned by `refresh`, this module only releases.

/// Return one-time parse arenas to the OS. Steady state is the lean
/// structures only. Shared by the server (initial build), the refresh
/// thread (post-swap retrim) and the database module (generation
/// rebuilds), which all build on the same path.
pub(crate) fn release_parse_arenas() {
  release_platform_arenas();
}

/// Linux (glibc/musl).
#[cfg(target_os = "linux")]
fn release_platform_arenas() {
  // SAFETY: malloc_trim only advises the allocator to release free pages;
  // it cannot invalidate live allocations, so it is always safe to call.
  unsafe {
    libc::malloc_trim(0);
  }
}

/// macOS: drain purgeable memory in all zones.
#[cfg(target_os = "macos")]
fn release_platform_arenas() {
  // Declared locally: libc 0.2 exposes only the zone-struct field, not
  // this stable Darwin function (malloc/malloc.h, present since 10.6).
  // `core::ffi` types keep this branch dependency-free (`libc` is only a
  // Linux dependency of this crate).
  unsafe extern "C" {
    /// Advisory purge of malloc zones (NULL zone = all zones, goal 0 = no target).
    fn malloc_zone_pressure_relief(zone: *mut core::ffi::c_void, goal: usize) -> usize;
  }
  // SAFETY: (NULL, 0) means "all zones, no goal" and is advisory-only;
  // it cannot invalidate live allocations.
  unsafe {
    malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
  }
}

/// Windows: compact our own process heap.
#[cfg(target_os = "windows")]
fn release_platform_arenas() {
  // SAFETY: HeapCompact with flags=0 only coalesces free blocks of the
  // given heap; it cannot invalidate live allocations.
  unsafe {
    let heap = winapi::um::heapapi::GetProcessHeap();
    if !heap.is_null() {
      winapi::um::heapapi::HeapCompact(heap, 0);
    }
  }
}

/// Other platforms: nothing to release through a stable API.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn release_platform_arenas() {}
