use super::*;

/// A staged clear rebuilds the exact command `clear_for_slot` produces.
/// This is the invariant that lets the queue stay slim without changing
/// what a wedged bridge eventually receives.
#[test]
fn slim_clear_rebuilds_the_same_command() {
  for pid in [0u64, 7, 42, u64::MAX] {
    let slot = slot(pid);
    let slim = PendingClear::from_slot(&slot);
    // JSON, not a struct compare: a changed field order or a dropped
    // field on either side shows up here.
    assert_eq!(
      serde_json::to_string(&slim.into_cmd()).expect("slim clear"),
      serde_json::to_string(&handlers::clear_for_slot(&slot)).expect("direct clear"),
      "pid {pid}: slim clear must rebuild byte-identically"
    );
  }
}

/// The slim form is a faithful inverse of the command it rebuilds, in
/// both directions. The flush relies on it: a refused clear is put back
/// as metadata taken from the command the sink handed it, so a clear that
/// survives any number of refusals must still rebuild the same bytes.
#[test]
fn slim_clear_round_trips_through_its_command() {
  for pid in [0u64, 7, 42, u64::MAX] {
    let slot = slot(pid);
    let slim = PendingClear::from_slot(&slot);
    let cmd = slim.clone().into_cmd();
    let back = PendingClear::from_cmd(&cmd);
    assert_eq!(back, slim, "pid {pid}: the slim form must round-trip");
  }
  // A nonce that is not null, and an id that is present: the inverse
  // reads both back out of the command rather than assuming the empty
  // slot.
  let slot = handlers::PublishedSlot {
    app_id: Some(adsum_types::AppId::from("app-7")),
    pid: 9,
    nonce: Value::String("n9".to_string()),
  };
  let slim = PendingClear::from_slot(&slot);
  let cmd = slim.clone().into_cmd();
  assert_eq!(PendingClear::from_cmd(&cmd), slim);
}

/// A staged clear is materially smaller than the command it replaces:
/// the whole point of holding slim metadata instead of the full command.
#[test]
fn slim_clear_is_smaller_than_the_command() {
  let cmd = big_publish(7);
  let clear = PendingClear::from_slot(&slot(7)).into_cmd();
  assert!(
    std::mem::size_of::<PendingClear>() < std::mem::size_of::<ActivityCmd>(),
    "metadata ({}) must be smaller than the command ({})",
    std::mem::size_of::<PendingClear>(),
    std::mem::size_of::<ActivityCmd>()
  );
  // And the activity the publish carried is genuinely gone.
  assert!(
    cmd
      .args
      .as_ref()
      .and_then(|a| a.activity.as_ref())
      .and_then(|a| a.as_ref())
      .is_some()
  );
  assert!(
    clear
      .args
      .as_ref()
      .is_some_and(|a| matches!(a.activity, Some(None)))
  );
}
