use super::*;

/// A pre-seeded slot list far past any plausible live process count, so
/// every tick is a low-occupancy tick on any machine.
fn surplus_list(count: usize) -> Vec<Exec> {
  (0..count)
    .map(|i| Exec {
      pid: i as u64 + 1,
      path: "p".repeat(64),
      arguments: Some("a".repeat(32)),
    })
    .collect()
}

/// Surplus slots survive below the threshold and are released exactly
/// when it is reached: a spike must not be punished for one quiet
/// moment, but a sustained drop must not pin slots forever.
#[test]
fn surplus_slots_are_released_only_after_sustained_low() {
  // 4x the threshold, and well past a realistic process count: the
  // premise (`live * 2 < slots`) holds on any machine this runs on.
  let surplus = SLOT_LOW_TICKS as usize * 512;
  let mut processes = surplus_list(surplus);
  let mut scratch = ScanScratch::default();

  let live = ProcessServer::process_list_into(&mut processes, &mut scratch).unwrap_or(0);
  if live * 2 >= surplus {
    // More live processes than the surplus (a huge host): the premise
    // fails, so this test has nothing to say. Never a false failure.
    return;
  }

  // Tick 1: the collapse is seen, nothing released yet.
  assert_eq!(processes.len(), surplus, "tick 1 must not release");
  assert_eq!(scratch.exec_scratch.low_ticks, 1);

  // Ticks 2..threshold: still retained.
  for expected in 2..SLOT_LOW_TICKS {
    ProcessServer::process_list_into(&mut processes, &mut scratch).expect("tick lists");
    assert_eq!(
      scratch.exec_scratch.low_ticks, expected,
      "counter climbs one per low tick"
    );
    assert_eq!(
      processes.len(),
      surplus,
      "surplus must survive under the threshold"
    );
  }

  // The threshold tick: released, and the counter resets. Released to
  // *this* tick's fill count, not the first tick's: the process count
  // drifts between ticks on a loaded host, so comparing against `live`
  // here would flake for a reason the test does not care about.
  let released_to =
    ProcessServer::process_list_into(&mut processes, &mut scratch).expect("release tick");
  assert_eq!(
    scratch.exec_scratch.low_ticks, 0,
    "counter resets after a release"
  );
  assert_eq!(
    processes.len(),
    released_to,
    "the list should sit at the live count"
  );
}

/// A healthy tick resets the counter, so spike-calm-spike-calm never
/// compacts.
#[test]
fn low_ticks_reset_on_a_healthy_tick() {
  // Two pre-seeded slots: the sweep fills them and grows to exactly its
  // own live count, so occupancy is high by construction.
  let mut processes = vec![Exec::default(), Exec::default()];
  let mut scratch = ScanScratch {
    exec_scratch: ExecScratch {
      low_ticks: 5,
      ..ExecScratch::default()
    },
    ..ScanScratch::default()
  };

  if let Ok(filled) = ProcessServer::process_list_into(&mut processes, &mut scratch) {
    assert!(filled >= 2, "the sweep must fill its own slots");
    // The list covers every filled slot and sits at most one slot ahead
    // of the fill cursor: `process_list_into` pushes a slot *before*
    // reading it, so a read that fails for the last directory entry
    // leaves `len == filled + 1`. That last case is not a bug and not
    // rare under load (a process exiting between `readdir` and its
    // `read`), which is why the previous `len == filled` assertion
    // flaked.
    assert!(
      processes.len() >= filled && processes.len() <= filled + 1,
      "list sits at the live count (plus at most one unreadable slot), got len={} filled={filled}",
      processes.len()
    );
    assert_eq!(
      scratch.exec_scratch.low_ticks, 0,
      "high occupancy resets the counter"
    );
  }
}
