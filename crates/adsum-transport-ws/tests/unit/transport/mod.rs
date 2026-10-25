use super::*;

mod clear_payload;
mod clear_retry;
mod connection;
mod pending_queue;
mod publish;

/// An activity with a sizable `details`, so a clear that still carried
/// one would dominate the entry's size.
fn activity_with_details() -> adsum_types::cmd::Activity {
  adsum_types::cmd::Activity {
    id: None,
    name: Some("G".to_string()),
    buttons: None,
    r#type: 0,
    url: None,
    created_at: None,
    session_id: None,
    platform: None,
    supported_platforms: None,
    timestamps: None,
    application_id: None,
    details: Some("d".repeat(1024)),
    details_url: None,
    state: None,
    state_url: None,
    sync_id: None,
    instance: None,
    status_display_type: None,
    flags: None,
    emoji: None,
    party: None,
    assets: None,
    secrets: None,
    metadata: None,
    extra: Default::default(),
  }
}

/// One published slot, as a disconnect would stage it.
fn slot(pid: u64) -> handlers::PublishedSlot {
  handlers::PublishedSlot {
    app_id: Some(AppId::from("app-1")),
    pid,
    nonce: Value::String(format!("n{pid}")),
  }
}

/// A `SET_ACTIVITY` big enough that a clear holding one would be
/// measurable (the point of the slim representation).
fn big_publish(pid: u64) -> ActivityCmd {
  ActivityCmd {
    application_id: Some(AppId::from("app-1")),
    cmd: "SET_ACTIVITY".into(),
    data: None,
    evt: None,
    args: Some(ActivityCmdArgs {
      pid: Some(pid),
      activity: Some(Some(activity_with_details())),
      code: None,
      user_id: None,
    }),
    nonce: Value::String(format!("n{pid}")),
  }
}
