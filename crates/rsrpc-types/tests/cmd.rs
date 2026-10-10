//! Hostile timestamps must saturate, never panic: debug builds
//! overflow-check arithmetic, so `i64::MIN` seconds times 1000 would
//! abort the daemon without saturation.

use rsrpc_types::cmd::ActivityCmd;

/// `i64::MIN` seconds would overflow `* 1000`: saturation keeps the
/// floor instead of panicking (debug) or wrapping (release).
#[test]
fn hostile_timestamps_saturate_instead_of_overflowing() {
  let mut cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "application_id": "app",
    "args": {"pid": 7, "activity": {"name": "G", "type": 0,
      "timestamps": {"start": i64::MIN, "end": i64::MIN}}},
    "nonce": "n",
  }))
  .unwrap();
  // Must return, not panic.
  cmd.fix_timestamps();
  let timestamps = cmd
    .args
    .as_ref()
    .unwrap()
    .activity
    .as_ref()
    .unwrap()
    .timestamps
    .as_ref()
    .unwrap();
  // `TimeoutValue.0` is crate-private: read through `Debug` instead.
  let rendered = format!(
    "{:?}",
    (
      timestamps.start.as_ref().unwrap(),
      timestamps.end.as_ref().unwrap()
    )
  );
  assert!(
    rendered.contains("-9223372036854775808"),
    "must saturate at the floor, got {rendered}"
  );
}

/// `fix_buttons` must preserve an existing `Metadata`: the old code rebuilt
/// the whole struct from a clone of itself to change one field, so any
/// future edit that drops the in-place merge silently loses metadata.
#[test]
fn fix_buttons_preserves_existing_metadata() {
  let mut cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "args": { "pid": 1, "activity": {
      "name": "G",
      "type": 0,
      "buttons": [{"label": "Play", "url": "https://example.com/play"}],
      "metadata": {
        "artist_ids": ["a1", "a2"],
        "album_id": "alb",
        "context_uri": "spotify:album:alb"
      }
    }},
    "nonce": "n"
  }))
  .expect("fixture parses");

  cmd.fix_buttons();

  let activity = cmd
    .args
    .as_ref()
    .and_then(|a| a.activity.as_ref())
    .expect("activity");
  let metadata = activity.metadata.as_ref().expect("metadata");
  // The derived field...
  assert_eq!(
    metadata.button_urls.as_deref(),
    Some(&["https://example.com/play".to_string()][..])
  );
  // ...and every pre-existing field, untouched.
  assert_eq!(
    metadata.artist_ids.as_deref(),
    Some(&["a1".to_string(), "a2".to_string()][..])
  );
  assert_eq!(metadata.album_id.as_deref(), Some("alb"));
  assert_eq!(metadata.context_uri.as_deref(), Some("spotify:album:alb"));
}

/// A button list with no urls must leave `metadata` exactly as it was
/// (absent stays absent): attaching an empty metadata object would change
/// what strict clients see.
#[test]
fn fix_buttons_without_urls_leaves_metadata_absent() {
  let mut cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "args": { "pid": 1, "activity": {"name": "G", "type": 0, "buttons": ["Just a label"]}},
    "nonce": "n"
  }))
  .expect("fixture parses");

  cmd.fix_buttons();

  let activity = cmd
    .args
    .as_ref()
    .and_then(|a| a.activity.as_ref())
    .expect("activity");
  assert!(
    activity.metadata.is_none(),
    "no urls means no metadata object"
  );
  assert_eq!(
    activity.buttons.as_ref().map(|b| b.len()),
    Some(1),
    "the label survives"
  );
}

/// Object buttons and plain-string buttons can coexist, and a non-string
/// `url` is skipped rather than fatal.
#[test]
fn fix_buttons_mixes_shapes_and_ignores_non_string_urls() {
  let mut cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "args": { "pid": 1, "activity": {
      "name": "G", "type": 0,
      "buttons": [
        "Plain",
        {"label": "Obj", "url": "https://ok.test"},
        {"label": "NoUrl"},
        {"label": "BadUrl", "url": 7}
      ]
    }},
    "nonce": "n"
  }))
  .expect("fixture parses");

  cmd.fix_buttons();

  let activity = cmd
    .args
    .as_ref()
    .and_then(|a| a.activity.as_ref())
    .expect("activity");
  let labels = activity.buttons.as_ref().expect("buttons");
  assert_eq!(labels.len(), 4, "every label kept: {labels:?}");
  assert_eq!(labels[0], serde_json::json!("Plain"));
  assert_eq!(labels[1], serde_json::json!("Obj"));
  assert_eq!(labels[2], serde_json::json!("NoUrl"));
  assert_eq!(labels[3], serde_json::json!("BadUrl"));
  assert_eq!(
    activity
      .metadata
      .as_ref()
      .and_then(|m| m.button_urls.as_deref()),
    Some(&["https://ok.test".to_string()][..]),
    "only the string url is kept"
  );
}

/// C3: the wire format is unchanged by carrying the ids as `AppId`.
///
/// `AppId` is serde-transparent over `Arc<str>`, so a command with and
/// without an outer id must serialize to the same bytes it always did —
/// this is a published contract (clients, persisted snapshots, the IPC
/// echo), and a type change that shifted a byte would be a silent
/// protocol break.
#[test]
fn application_ids_keep_their_wire_bytes() {
  let cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "application_id": "12345",
    "args": {"pid": 7, "activity": {"name": "G", "type": 0,
      "application_id": "67890"}},
    "nonce": "n",
  }))
  .unwrap();

  // Outer id: present, as a plain JSON string.
  assert_eq!(cmd.application_id.as_deref(), Some("12345"));
  // Inner (activity) id likewise.
  let activity = cmd.args.as_ref().unwrap().activity.as_ref().unwrap();
  assert_eq!(activity.application_id.as_deref(), Some("67890"));

  // Byte-exact round trip: a missing id stays omitted, not `null`.
  let json = serde_json::to_string(&cmd).unwrap();
  assert!(
    json.contains(r#""application_id":"12345""#),
    "outer id wire shape changed: {json}"
  );
  let back: ActivityCmd = serde_json::from_str(&json).unwrap();
  assert_eq!(back.application_id.as_deref(), Some("12345"));
  assert_eq!(
    back
      .args
      .as_ref()
      .unwrap()
      .activity
      .as_ref()
      .unwrap()
      .application_id
      .as_deref(),
    Some("67890")
  );

  // Absent on both sides stays absent.
  let bare: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "args": {"pid": 7, "activity": {"name": "G", "type": 0}},
    "nonce": "n",
  }))
  .unwrap();
  assert_eq!(bare.application_id, None);
  let bare_json = serde_json::to_string(&bare).unwrap();
  assert!(
    !bare_json.contains("application_id"),
    "an absent id must stay omitted: {bare_json}"
  );
}

/// C3: the id a command carries is shared, not copied, when the command
/// is cloned.
///
/// This is the point of the migration: the per-publish clone used to deep
/// -copy the application id, and the stamps in the protocol layer copied
/// it again. Refcounted ids make both free.
#[test]
fn command_clones_share_the_application_id() {
  let cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "application_id": "123456789012345678",
    "args": {"pid": 7, "activity": {"name": "G", "type": 0,
      "application_id": "876543210987654321"}},
    "nonce": "n",
  }))
  .unwrap();
  let outer_ptr = cmd.application_id.as_ref().unwrap().as_ptr();
  let inner_ptr = cmd
    .args
    .as_ref()
    .unwrap()
    .activity
    .as_ref()
    .unwrap()
    .application_id
    .as_ref()
    .unwrap()
    .as_ptr();

  let cloned = cmd.clone();

  assert!(
    std::ptr::eq(cloned.application_id.as_ref().unwrap().as_ptr(), outer_ptr),
    "cloning a command must share the outer application id, never copy it"
  );
  assert!(
    std::ptr::eq(
      cloned
        .args
        .as_ref()
        .unwrap()
        .activity
        .as_ref()
        .unwrap()
        .application_id
        .as_ref()
        .unwrap()
        .as_ptr(),
      inner_ptr
    ),
    "cloning a command must share the activity's application id too"
  );
}
