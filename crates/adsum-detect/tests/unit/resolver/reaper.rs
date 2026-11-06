use super::*;
use crate::resolver::resolve_steam_reaper;

fn exec(path: &str, args: Option<&str>) -> Exec {
  Exec {
    pid: 1000,
    path: path.into(),
    arguments: args.map(str::to_string),
  }
}

#[test]
fn reaper_resolves_app_id_and_target() {
  let e = exec("reaper", Some("SteamLaunch AppId=892970 -- ./game"));
  let r = resolve_steam_reaper(&e).expect("resolves");
  assert_eq!(r.0, 892970);
  assert_eq!(r.1, 4); // index of "./game" after "--"
}

#[test]
fn reaper_rejects_missing_steam_launch() {
  let e = exec("reaper", Some("AppId=892970 -- ./game"));
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_rejects_missing_separator() {
  let e = exec("reaper", Some("SteamLaunch AppId=892970 ./game"));
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_rejects_empty_app_id() {
  let e = exec("reaper", Some("SteamLaunch AppId= -- ./game"));
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_rejects_non_numeric_app_id() {
  let e = exec("reaper", Some("SteamLaunch AppId=abc -- ./game"));
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_rejects_no_arguments() {
  let e = exec("reaper", None);
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_rejects_non_reaper_path() {
  let e = exec("steam", Some("SteamLaunch AppId=892970 -- ./game"));
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_rejects_target_past_argv() {
  let e = exec("reaper", Some("SteamLaunch AppId=892970 --"));
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_rejects_space_app_id() {
  let e = exec("reaper", Some("SteamLaunch AppId 892970 -- ./game"));
  assert!(resolve_steam_reaper(&e).is_none());
}

#[test]
fn reaper_end_to_end_app_id_and_target() {
  let e = exec("reaper", Some("SteamLaunch AppId=440 -- ./game -arg"));
  let r = resolve_steam_reaper(&e).expect("resolves");
  assert_eq!(r.0, 440);
  // target index points at ./game in argv (1-based).
  assert_eq!(r.1, 4);
}
