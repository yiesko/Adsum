use super::*;
use crate::server::ProcessServer;

/// Own pid is always readable: deterministic fixture, no /proc guessing.
fn self_pid() -> u64 {
  u64::from(std::process::id())
}

/// `read_exec_into` classifies exactly like `read_exec`: same presence,
/// same pid, same path, same arguments.
#[test]
fn read_exec_into_matches_read_exec() {
  for pid in [self_pid(), 1] {
    let mut slot = Exec::default();
    let mut scratch = ExecScratch::default();
    let present = read_exec_into(pid, &mut slot, &mut scratch);
    match read_exec(pid) {
      None => assert!(!present, "into() must agree on unreadable pid {pid}"),
      Some(expected) => {
        assert!(present, "into() must agree on readable pid {pid}");
        assert_eq!(slot.pid, expected.pid);
        assert_eq!(slot.path, expected.path);
        assert_eq!(slot.arguments, expected.arguments);
      }
    }
  }
  // Out-of-range pid: both agree on absence without touching buffers.
  let mut slot = Exec::default();
  let mut scratch = ExecScratch::default();
  assert!(!read_exec_into(u64::MAX, &mut slot, &mut scratch));
  assert!(read_exec(u64::MAX).is_none());
}

/// A second call with the same pid must not reallocate: scratch pointers
/// and capacities are stable, slot strings keep their buffers.
#[test]
fn read_exec_into_reuses_buffers() {
  let pid = self_pid();
  let mut slot = Exec::default();
  let mut scratch = ExecScratch::default();
  assert!(read_exec_into(pid, &mut slot, &mut scratch));
  let path_ptr = slot.path.as_ptr();
  let path_cap = slot.path.capacity();
  let cmd_ptr = scratch.cmdline.as_ptr();
  let cmd_cap = scratch.cmdline.capacity();
  assert!(read_exec_into(pid, &mut slot, &mut scratch));
  assert!(
    std::ptr::eq(path_ptr, slot.path.as_ptr()),
    "path buffer moved"
  );
  assert!(
    std::ptr::eq(cmd_ptr, scratch.cmdline.as_ptr()),
    "cmdline buffer moved"
  );
  assert!(slot.path.capacity() >= path_cap);
  assert!(scratch.cmdline.capacity() >= cmd_cap);
}

/// D5: `read_steam_app_id` reads environ into the scratch's own buffer
/// and builds its path in the scratch's path buffer, so a miss — the
/// common case, every non-Steam process — costs no allocation at all
/// beyond the returned `Option`.
///
/// environ is kilobytes and the only read of the scan that big: it
/// runs once per newly-seen pid, but a fresh `Vec` (plus the `format!`
/// of the path) was paid every time, with the buffer growing back up
/// to its real size each call instead of keeping the capacity.
///
/// The reuse check is about capacity, not pointers: `read_to_end`
/// legitimately grows the buffer on the first call, so the assertion is
/// that the second call neither reallocates nor shrinks it. `path` is
/// the stronger half — same `String`, refilled with `write!`.
#[test]
fn steam_app_id_read_reuses_the_exec_scratch() {
  // Self pid is deterministic (present, readable) and its own environ
  // is irrelevant: what matters is that both buffers survive the call.
  // Whether it carries a SteamAppId is environment-dependent, so the
  // assertions below are the reuse contract, not the lookup result.
  let pid = self_pid();
  let mut scratch = ExecScratch::default();

  let _ = read_steam_app_id(pid, &mut scratch);
  let path_ptr = scratch.path.as_ptr();
  let path_cap = scratch.path.capacity();
  let env_ptr = scratch.environ.as_ptr();
  let env_cap = scratch.environ.capacity();

  let _ = read_steam_app_id(pid, &mut scratch);

  assert!(
    std::ptr::eq(path_ptr, scratch.path.as_ptr()),
    "path buffer moved: the environ read must reuse it"
  );
  assert!(
    std::ptr::eq(env_ptr, scratch.environ.as_ptr()),
    "environ buffer moved: the read must reuse it"
  );
  assert!(scratch.path.capacity() >= path_cap, "path buffer shrank");
  assert!(scratch.environ.capacity() >= env_cap, "environ shrank");
  // The reuse target is the scratch, so it must have been touched: an
  // implementation that falls back to its own buffers leaves these
  // empty while still passing every pointer check above.
  assert_eq!(
    scratch.path,
    format!("/proc/{pid}/environ"),
    "the read must build its path in the scratch"
  );
  assert!(
    !scratch.environ.is_empty(),
    "the read must land its bytes in the scratch"
  );
}

/// `process_list_into` reuses the Vec backing and every slot across
/// ticks: same allocation, refreshed contents. The first-slot pointer
/// check is conditional on pid stability: if a process exits between
/// the two back-to-back sweeps, readdir order may shift and the slot
/// legitimately holds another pid. The backing-pointer check is
/// conditional on capacity: growth past the first high-water mark may
/// reallocate, which is correct, not a reuse bug. Surplus slots past
/// `filled` are retained (high-water), so assertions only read the
/// filled prefix.
#[test]
fn process_list_into_reuses_slots_across_ticks() {
  let mut processes = Vec::new();
  let mut scratch = ExecScratch::default();
  let filled =
    ProcessServer::process_list_into(&mut processes, &mut scratch).expect("first tick lists");
  assert!(filled > 0, "some process must be visible");
  assert!(filled <= processes.len());
  assert!(
    processes[..filled]
      .iter()
      .all(|e| e.pid > 0 && !e.path.is_empty())
  );
  let backing_ptr = processes.as_ptr();
  let backing_cap = processes.capacity();
  let (first_pid, first_path_ptr, first_path_cap) = (
    processes[0].pid,
    processes[0].path.as_ptr(),
    processes[0].path.capacity(),
  );
  let filled2 =
    ProcessServer::process_list_into(&mut processes, &mut scratch).expect("second tick lists");
  assert!(filled2 > 0);
  assert!(filled2 <= processes.len());
  if processes.len() <= backing_cap {
    assert!(
      std::ptr::eq(backing_ptr, processes.as_ptr()),
      "Vec backing moved"
    );
  }
  if processes[0].pid == first_pid {
    assert!(
      std::ptr::eq(first_path_ptr, processes[0].path.as_ptr()),
      "slot string buffer moved"
    );
    assert!(processes[0].path.capacity() >= first_path_cap);
  }
  // High-water retention: the second sweep never shrinks the Vec, so a
  // later rise reuses surplus slot buffers instead of reallocating.
  assert!(processes.len() >= filled);
  assert!(processes.len() >= filled2);
}

/// D6: the suspended check parses `/proc/<pid>/stat` into the scratch's
/// own buffer instead of building a fresh `String` per hit.
///
/// The read is tiny (`stat` is a few hundred bytes) but it runs for
/// every aux hit — 0 to 3 per tick — and the previous code paid two
/// allocations each time: a `String::new()` plus the `format!` of the
/// path. Letting the scratch own the buffer makes repeat checks free.
///
/// The empty-buffer case matters as much as the steady state: a check
/// that fails to fill the scratch (unreadable pid) must leave it clear,
/// not leave last tick's bytes behind for the next parse to misread.
#[cfg(target_os = "linux")]
#[test]
fn stat_scratch_is_reused_across_suspension_checks() {
  let pid = u64::from(std::process::id());
  let mut scratch = MatchScratch::default();

  // A readable pid fills the scratch; that is the reuse target.
  let _ = is_suspended(pid, &mut scratch.stat);
  assert!(
    !scratch.stat.is_empty(),
    "the check must parse into the scratch buffer"
  );
  let stat_ptr = scratch.stat.as_ptr();
  let stat_cap = scratch.stat.capacity();

  for _ in 0..3 {
    let _ = is_suspended(pid, &mut scratch.stat);
    assert!(
      std::ptr::eq(stat_ptr, scratch.stat.as_ptr()),
      "stat buffer moved"
    );
    assert!(scratch.stat.capacity() >= stat_cap, "stat buffer shrank");
  }

  // An unreadable pid reuses the same buffer and clears it, so a later
  // check can never parse the previous pid's line.
  let _ = is_suspended(u64::MAX, &mut scratch.stat);
  assert!(
    std::ptr::eq(stat_ptr, scratch.stat.as_ptr()),
    "failed checks must not reallocate"
  );
  assert!(
    scratch.stat.is_empty(),
    "a failed read must clear the scratch, not leave stale bytes"
  );
}
